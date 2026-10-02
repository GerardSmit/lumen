//! The thunk + descriptor of one bindable fn (an `#[op]` or a class member): a `Native<H>` impl
//! generic over the host, whose where-clause is exactly what the fn needs from a host.

use crate::parse::*;

pub const B: &str = "::lumen_bind";
const P: &str = "::lumen_bind::__private";

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Role {
    Function,
    Method,
    Static,
    Getter,
    Setter,
    Constructor,
    Proto(String),
}

impl Role {
    fn expr(&self) -> String {
        match self {
            Role::Function => format!("{B}::Role::Function"),
            Role::Method => format!("{B}::Role::Method"),
            Role::Static => format!("{B}::Role::Static"),
            Role::Getter => format!("{B}::Role::Getter"),
            Role::Setter => format!("{B}::Role::Setter"),
            Role::Constructor => format!("{B}::Role::Constructor"),
            Role::Proto(p) => format!("{B}::Role::Proto({p:?})"),
        }
    }
    fn has_receiver(&self) -> bool {
        matches!(self, Role::Method | Role::Getter | Role::Setter | Role::Proto(_))
    }
}

/// One fn to generate.
pub struct Spec<'a> {
    pub sig: &'a Sig,
    pub role: Role,
    pub opts: &'a Opts,
    /// The class (members only).
    pub self_ty: Option<&'a str>,
    /// The Rust fn to call (`super::clamp`, `<Deque>::append`).
    pub call_path: String,
    pub owner: String,
    /// Item names: the descriptor static, the `Native` struct, the scalar entry fn.
    pub desc: String,
    pub op: String,
    pub scalar: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    PosOnly,
    PosOrKw,
    KwOnly,
    VarArgs,
    VarKw,
}

enum Use {
    Ctx,
    State { inner: String, mutable: bool },
    This(Vec<proc_macro::TokenTree>),
    /// A named parameter (index among the named ones).
    Arg { idx: usize, default: Option<String> },
    Rest,
    Kw,
}

fn is_ctx(ts: &str) -> Option<&str> {
    let inner = ts.strip_prefix("&mut ")?;
    let b = base_name(inner);
    (b.ends_with("Ctx") || b.ends_with("Interp")).then_some(inner)
}

fn scalar_kind(ts: &str) -> Option<&'static str> {
    match ts {
        "f64" => Some("F64"),
        "i32" => Some("I32"),
        "u32" => Some("U32"),
        "bool" => Some("Bool"),
        _ => None,
    }
}

/// Converted after the plain arguments: byte slices and class borrows (so nothing that may run
/// script code follows a borrow).
fn is_late(ts: &str) -> bool {
    let t = ts.strip_prefix("Option<").and_then(|t| t.strip_suffix('>')).unwrap_or(ts);
    if t.starts_with("&[") || t.starts_with("&mut [") {
        return true;
    }
    let Some(inner) = t.strip_prefix("&mut ").or_else(|| t.strip_prefix('&')) else { return false };
    let b = base_name(inner);
    !matches!(b, "str" | "Value" | "Obj" | "String") && b.chars().next().is_some_and(|c| c.is_ascii_uppercase())
}

/// Parameter types whose `#[default(..)]` literal is used as is.
const PRIMITIVES: &[&str] = &[
    "f64", "f32", "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "isize", "usize", "i128", "u128", "bool", "char", "&str",
];

fn opt_str(o: Option<&str>) -> String {
    match o {
        Some(s) => format!("::core::option::Option::Some({s:?})"),
        None => "::core::option::Option::None".into(),
    }
}

fn str_list(v: &[String]) -> String {
    format!("&[{}]", v.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", "))
}

pub fn names_fields(opts: &Opts) -> String {
    let renames = opts.map("rename").iter().map(|(h, n)| format!("({h:?}, {n:?})")).collect::<Vec<_>>().join(", ");
    let hints = opts.hints.iter().map(|(h, k, v)| format!("({h:?}, {k:?}, {v:?})")).collect::<Vec<_>>().join(", ");
    format!(
        "explicit: {}, renames: &[{renames}], only: {}, skip: {}, hints: &[{hints}]",
        opt_str(opts.get("name")),
        str_list(&opts.list("only")),
        str_list(&opts.list("skip"))
    )
}

/// The generated items (as source text).
pub fn gen(s: &Spec) -> Res<String> {
    let sig = s.sig;
    let span = sig.name_span;
    let self_ty = s.self_ty;
    let coerce = s.opts.has("coerce");
    let is_async = s.opts.has("async");

    // Classify the parameters.
    let mut uses = Vec::new();
    let mut named: Vec<(usize, Kind)> = Vec::new();
    let mut has_this = false;
    let mut ctx_ty: Option<String> = None;
    let mut state = false;
    for (k, p) in sig.params.iter().enumerate() {
        let attr_names: Vec<&str> = p.attrs.iter().map(|a| a.path.as_str()).filter(|n| PARAM_ATTRS.contains(n)).collect();
        if let Some(inner) = is_ctx(&p.ts) {
            if ctx_ty.is_some() {
                return Err((p.span, "at most one `&mut Ctx` parameter".into()));
            }
            let _ = inner;
            ctx_ty = Some(render(&p.ty[2..], self_ty, Lt::Infer));
            uses.push(Use::Ctx);
            continue;
        }
        if base_name(&p.ts) == "State" && p.ts.starts_with('&') {
            let mutable = p.ts.starts_with("&mut ");
            let inner = generic_inner(&p.ty).ok_or((p.span, "expected `&State<T>`".to_string()))?;
            state = true;
            uses.push(Use::State { inner: render(&inner, self_ty, Lt::Infer), mutable });
            continue;
        }
        if base_name(&p.ts) == "This" && !p.ts.starts_with('&') {
            let inner = generic_inner(&p.ty).ok_or((p.span, "expected `This<T>`".to_string()))?;
            has_this = true;
            uses.push(Use::This(inner));
            continue;
        }
        let kind = if attr_names.contains(&"varargs") {
            Kind::VarArgs
        } else if attr_names.contains(&"varkw") {
            Kind::VarKw
        } else if attr_names.contains(&"kwonly") {
            Kind::KwOnly
        } else if attr_names.contains(&"kw") {
            Kind::PosOrKw
        } else {
            Kind::PosOnly
        };
        let default = p.attrs.iter().find(|a| a.path == "default").map(|a| a.args_ts().to_string());
        match kind {
            Kind::VarArgs => uses.push(Use::Rest),
            Kind::VarKw => uses.push(Use::Kw),
            _ => {
                uses.push(Use::Arg { idx: named.len(), default });
                named.push((k, kind));
            }
        }
        // Order: posonly, pos-or-kw, *args, kwonly, **kwargs.
        let rank = |k: Kind| match k {
            Kind::PosOnly => 0,
            Kind::PosOrKw => 1,
            Kind::VarArgs => 2,
            Kind::KwOnly => 3,
            Kind::VarKw => 4,
        };
        let prev = sig.params[..k]
            .iter()
            .filter_map(|q| {
                let names: Vec<&str> = q.attrs.iter().map(|a| a.path.as_str()).collect();
                if is_ctx(&q.ts).is_some() || matches!(base_name(&q.ts), "State" | "This") {
                    None
                } else if names.contains(&"varargs") {
                    Some(Kind::VarArgs)
                } else if names.contains(&"varkw") {
                    Some(Kind::VarKw)
                } else if names.contains(&"kwonly") {
                    Some(Kind::KwOnly)
                } else if names.contains(&"kw") {
                    Some(Kind::PosOrKw)
                } else {
                    Some(Kind::PosOnly)
                }
            })
            .map(rank)
            .max();
        if prev.is_some_and(|r| r > rank(kind) || (r == rank(kind) && matches!(kind, Kind::VarArgs | Kind::VarKw))) {
            return Err((p.span, "parameter order must be: positional-only, `#[kw]`, `#[varargs]`, `#[kwonly]`, `#[varkw]`".into()));
        }
    }
    if ctx_ty.is_some() && state {
        return Err((span, "a fn takes `&mut Ctx` or `State<T>`, not both".into()));
    }
    let receiver = sig.receiver;
    if s.role.has_receiver() && receiver.is_none() && !has_this {
        return Err((span, "an instance member takes `&self`, `&mut self` or a `This<_>` parameter".into()));
    }
    if !s.role.has_receiver() && receiver.is_some() {
        return Err((span, "this kind of member takes no receiver".into()));
    }
    if is_async && (receiver.is_some() || ctx_ty.is_some() || state || has_this) {
        return Err((span, "an `async` fn takes owned arguments only (no receiver, `&mut Ctx` or state)".into()));
    }
    match s.role {
        Role::Getter if !named.is_empty() => return Err((span, "a getter takes no arguments".into())),
        Role::Setter if named.len() != 1 => return Err((span, "a setter takes exactly one argument".into())),
        _ => {}
    }

    // Optional parameters: a default, or an `Option<T>` / `Passed<T>` with nothing required
    // after it among the positional ones (a keyword-only one is always optional).
    let is_option = |k: usize| matches!(base_name(&sig.params[k].ts), "Option" | "Passed");
    let none_text = |k: usize| if base_name(&sig.params[k].ts) == "Passed" { "<unrepresentable>" } else { "None" };
    let mut optional = vec![false; named.len()];
    let mut default_text: Vec<Option<String>> = vec![None; named.len()];
    for u in &uses {
        if let Use::Arg { idx, default: Some(d) } = u {
            optional[*idx] = true;
            default_text[*idx] = Some(d.clone());
        }
    }
    let mut tail_ok = true;
    for i in (0..named.len()).rev() {
        let (k, kind) = named[i];
        if kind == Kind::KwOnly {
            if is_option(k) && default_text[i].is_none() {
                optional[i] = true;
                default_text[i] = Some(none_text(k).into());
            }
            continue;
        }
        if optional[i] {
            continue;
        }
        if is_option(k) && tail_ok {
            optional[i] = true;
            default_text[i] = Some(none_text(k).into());
        } else {
            tail_ok = false;
        }
    }
    let max_pos = named.iter().filter(|(_, k)| matches!(k, Kind::PosOnly | Kind::PosOrKw)).count();
    let min_pos = (0..max_pos).take_while(|&i| !optional[i]).count();

    // The descriptor.
    let mut params_src = Vec::new();
    for (k, p) in sig.params.iter().enumerate() {
        let (kind, default) = match &uses[k] {
            Use::Arg { idx, .. } => {
                let kind = match named[*idx].1 {
                    Kind::PosOnly => "PosOnly",
                    Kind::PosOrKw => "PosOrKw",
                    _ => "KwOnly",
                };
                (kind, default_text[*idx].clone())
            }
            Use::Rest => ("VarArgs", None),
            Use::Kw => ("VarKw", None),
            _ => continue,
        };
        params_src.push(format!(
            "{B}::Param {{ name: {:?}, kind: {B}::ParamKind::{kind}, default: {} }}",
            p.name.trim_start_matches("r#").trim_start_matches('_'),
            opt_str(default.as_deref())
        ));
    }
    let mut flags = Vec::new();
    if coerce {
        flags.push(format!("{B}::flags::COERCE"));
    }
    if ctx_ty.is_some() {
        flags.push(format!("{B}::flags::CTX"));
    }
    if is_async {
        flags.push(format!("{B}::flags::ASYNC"));
    }
    if receiver == Some(Receiver::Mut) {
        flags.push(format!("{B}::flags::MUT_SELF"));
    }
    if state {
        flags.push(format!("{B}::flags::STATE"));
    }
    if s.opts.has("__classmethod") {
        flags.push(format!("{B}::flags::CLASS_RECV"));
    }
    let flags = if flags.is_empty() { "0".to_string() } else { flags.join(" | ") };

    let ret_ts = sig.ret.as_ref().map(|r| type_str(r)).unwrap_or_else(|| "()".into());
    let ret_infer = sig.ret.as_ref().map(|r| render(r, self_ty, Lt::Infer)).unwrap_or_else(|| "()".into());
    let ret_named = sig.ret.as_ref().map(|r| render(r, self_ty, Lt::Named("'a"))).unwrap_or_else(|| "()".into());

    // The unboxed entry.
    let mut items = String::new();
    let scalar_ok = s.role == Role::Function
        && !is_async
        && !coerce
        && receiver.is_none()
        && uses.iter().all(|u| matches!(u, Use::Arg { default: None, .. }))
        && named.iter().all(|(k, _)| scalar_kind(&sig.params[*k].ts).is_some())
        && (ret_ts == "()" || scalar_kind(&ret_ts).is_some());
    let scalar = if scalar_ok {
        let mut ps = Vec::new();
        let mut call = Vec::new();
        let mut kinds = Vec::new();
        for (n, (k, _)) in named.iter().enumerate() {
            let kind = scalar_kind(&sig.params[*k].ts).unwrap();
            kinds.push(format!("{B}::Scalar::{kind}"));
            if kind == "Bool" {
                ps.push(format!("a{n}: i32"));
                call.push(format!("a{n} != 0"));
            } else {
                ps.push(format!("a{n}: {}", sig.params[*k].ts));
                call.push(format!("a{n}"));
            }
        }
        let (rk, rty, conv) = match scalar_kind(&ret_ts) {
            None => ("Void", "()", ""),
            Some("Bool") => ("Bool", "i32", " as i32"),
            Some(k) => (k, ret_ts.as_str(), ""),
        };
        items.push_str(&format!(
            "extern \"C\" fn {}({}) -> {rty} {{ {}({}){conv} }}\n",
            s.scalar,
            ps.join(", "),
            s.call_path,
            call.join(", ")
        ));
        format!(
            "::core::option::Option::Some({B}::ScalarEntry {{ args: &[{}], ret: {B}::Scalar::{rk}, ptr: {B}::CodePtr({} as *const ()) }})",
            kinds.join(", "),
            s.scalar
        )
    } else {
        "::core::option::Option::None".into()
    };
    items.push_str(&format!(
        "pub static {}: {B}::FnDesc = {B}::FnDesc {{ name: {:?}, {}, owner: {}, role: {}, doc: {}, params: &[{}], \
         max_pos: {max_pos}, min_pos: {min_pos}, flags: {flags}, scalar: {scalar} }};\n",
        s.desc,
        sig.name.strip_prefix("r#").unwrap_or(&sig.name),
        names_fields(s.opts),
        s.owner,
        s.role.expr(),
        opt_str(sig.doc.as_deref()),
        params_src.join(", "),
    ));

    // Bounds and the body.
    let mut bounds: Vec<String> = Vec::new();
    let mut callbacks = Vec::new();
    let mut conv_early = String::new();
    let mut conv_late = String::new();
    let mut call_args = Vec::new();
    let tys: Vec<String> = sig.params.iter().map(|p| render(&p.ty, self_ty, Lt::Infer)).collect();
    for (k, p) in sig.params.iter().enumerate() {
        let ty = &tys[k];
        let ty_a = render(&p.ty, self_ty, Lt::Named("'a"));
        match &uses[k] {
            Use::Ctx => call_args.push("__ctx".to_string()),
            Use::State { mutable, .. } => call_args.push(if *mutable { "__st".into() } else { "&*__st".into() }),
            Use::This(inner) => {
                let inner_infer = render(inner, self_ty, Lt::Infer);
                let inner_a = render(inner, self_ty, Lt::Named("'a"));
                bounds.push(format!("for<'a> {inner_a}: {B}::FromArg<'a, H>"));
                conv_late.push_str(&format!("let __a{k}: {ty} = {P}::this::<H, {inner_infer}>(__cx)?;\n"));
                call_args.push(format!("__a{k}"));
            }
            Use::Arg { idx, default } => {
                bounds.push(format!("for<'a> {ty_a}: {B}::FromArg<'a, H>"));
                let ty_static = render(&p.ty, self_ty, Lt::Named("'static"));
                callbacks.push(format!("{P}::callback_bit(<{ty_static} as {B}::FromArg<'static, H>>::CALLBACK, {idx})"));
                let at = format!("{B}::Slot::arg({idx})");
                let code = match default {
                    None => format!("let __a{k}: {ty} = {P}::arg::<H, {ty}>(__cx, __s{idx}, {at})?;\n"),
                    Some(d) => {
                        // Literals of primitive parameters are used as written; other types
                        // (a host's value type) convert from the literal with `Into`.
                        let opt_inner = p.ts.strip_prefix("Option<").and_then(|t| t.strip_suffix('>'));
                        let lit = |t: &str| {
                            if PRIMITIVES.contains(&t) {
                                d.clone()
                            } else {
                                format!("::core::convert::Into::into({d})")
                            }
                        };
                        let dexpr = match opt_inner {
                            Some(_) if d.trim() == "None" => "::core::option::Option::None".to_string(),
                            Some(inner) => format!("::core::option::Option::Some({})", lit(inner)),
                            None => lit(&p.ts),
                        };
                        format!(
                            "let __a{k}: {ty} = match __s{idx} {{ ::core::option::Option::Some(__v) => <{ty} as {B}::FromArg<'_, H>>::from_arg(__cx, __v, {at})?, ::core::option::Option::None => {dexpr} }};\n"
                        )
                    }
                };
                if is_late(&p.ts) {
                    conv_late.push_str(&code);
                    let later_early = sig.params[k + 1..]
                        .iter()
                        .zip(&uses[k + 1..])
                        .any(|(q, u)| matches!(u, Use::Arg { .. } | Use::Rest | Use::Kw) && !is_late(&q.ts));
                    if later_early {
                        conv_early.push_str(&format!(
                            "if let ::core::option::Option::Some(__v) = __s{idx} {{ <{ty} as {B}::FromArg<'_, H>>::reserve(__cx, __v, {at})?; }}\n"
                        ));
                    }
                } else {
                    conv_early.push_str(&code);
                }
                call_args.push(format!("__a{k}"));
            }
            Use::Rest => {
                bounds.push(format!("for<'a> {ty_a}: {B}::FromRest<'a, H>"));
                conv_early.push_str(&format!("let __a{k}: {ty} = {P}::rest::<H, {ty}>(__cx, {max_pos})?;\n"));
                call_args.push(format!("__a{k}"));
            }
            Use::Kw => {
                bounds.push(format!("for<'a> {ty_a}: {B}::FromVarKw<'a, H>"));
                conv_early.push_str(&format!("let __a{k}: {ty} = {P}::varkw::<H, {ty}>(__cx)?;\n"));
                call_args.push(format!("__a{k}"));
            }
        }
    }
    if let Some(ct) = &ctx_ty {
        bounds.push(format!("H: {B}::Host<Ctx = {ct}>"));
    }
    if state {
        bounds.push(format!("H: {B}::StateHost"));
    }
    let n = named.len();
    let slots = (0..n).map(|i| format!("__s{i}")).collect::<Vec<_>>().join(", ");
    let mut body = format!("let [{slots}] = H::bind::<{n}>(__cx)?;\n");
    body.push_str(&conv_early);
    body.push_str(&conv_late);
    if let Some(r) = receiver {
        let ty = self_ty.unwrap();
        let f = if r == Receiver::Mut { "class_mut" } else { "class_ref" };
        body.push_str(&format!("let __self = H::{f}::<{ty}>(__cx, H::this(__cx), {B}::Slot::THIS)?;\n"));
        call_args.insert(0, "__self".into());
    }
    let call = format!("{}({})", s.call_path, call_args.join(", "));
    let call = if ctx_ty.is_some() {
        format!("H::with_ctx(__cx, |__ctx| {call})")
    } else if let Some(Use::State { inner, .. }) = uses.iter().find(|u| matches!(u, Use::State { .. })) {
        format!("<H as {B}::StateHost>::with_state::<{inner}, _>(__cx, |__st| {call})?")
    } else {
        call
    };
    let tail = if is_async {
        bounds.push(format!("H: {B}::SpawnHost"));
        let ret_static = sig.ret.as_ref().map(|r| render(r, self_ty, Lt::Named("'static"))).unwrap_or_else(|| "()".into());
        bounds.push(format!("{ret_static}: {B}::IntoRet<H> + ::core::marker::Send + 'static"));
        body.push_str(&format!("<H as {B}::SpawnHost>::spawn_blocking(__cx, move || {call})\n"));
        body = format!(
            "let __r = (|| -> ::core::result::Result<H::Value, H::Error> {{\n{body}}})();\n<H as {B}::SpawnHost>::async_ret(__cx, __r)\n"
        );
        String::new()
    } else {
        match &s.role {
            Role::Constructor => {
                let ty = self_ty.unwrap();
                bounds.push(format!("{ret_infer}: {B}::CtorRet<H, {ty}>"));
                format!("let __r = {call};\n{P}::ctor::<H, {ty}, _>(__cx, __r)\n")
            }
            Role::Proto(p) if p == "next" => {
                bounds.push(format!("for<'a> {ret_named}: {B}::NextRet<H>"));
                format!("let __r = {call};\nH::ret_next(__cx, __r)\n")
            }
            _ => {
                bounds.push(format!("for<'a> {ret_named}: {B}::IntoRet<H>"));
                format!("let __r = {call};\nH::ret(__cx, __r)\n")
            }
        }
    };
    body.push_str(&tail);
    let callbacks = if callbacks.is_empty() { "0".to_string() } else { callbacks.join(" | ") };
    items.push_str(&format!("pub struct {};\n", s.op));
    items.push_str(&format!(
        "#[allow(private_bounds, clippy::all)]\n\
         impl<H: {B}::Host> {B}::Native<H> for {} where {} {{\n\
           const DESC: &'static {B}::FnDesc = &{};\n\
           const CALLBACKS: u32 = {callbacks};\n\
           #[inline]\n\
           fn call(__cx: &H::Cx<'_>) -> ::core::result::Result<H::Value, H::Error> {{\n{body}}}\n\
         }}\n",
        s.op,
        bounds.join(",\n"),
        s.desc
    ));
    Ok(items)
}
