//! The attribute macros of `lumen-bind` (see that crate for the syntax). They emit neutral
//! descriptors and thunks generic over `::lumen_bind::Host`; nothing here knows a language.

mod gen;
mod parse;

use gen::{names_fields, Role, Spec, B};
use parse::*;
use proc_macro::{Delimiter, Span, TokenStream, TokenTree};

/// A bindable fn (see the `lumen_bind` crate docs).
#[proc_macro_attribute]
pub fn op(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_op(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[op]: {msg}")),
    }
}

/// A bindable struct; pair it with `#[methods]` on its impl block.
#[proc_macro_attribute]
pub fn class(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_class(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[class]: {msg}")),
    }
}

/// The members of a `#[class]`.
#[proc_macro_attribute]
pub fn methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_methods(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[methods]: {msg}")),
    }
}

/// A module: registers every `#[op]`, `#[class]`, `#[constant]` and `#[init]` it contains.
#[proc_macro_attribute]
pub fn module(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_module(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[module]: {msg}")),
    }
}

const FN_OPTS: &[&str] = &["name", "rename", "only", "skip", "coerce", "async", "hint"];

fn owner_of(opts: &Opts) -> String {
    match opts.get("__module") {
        Some(m) => format!("{B}::Owner::Module({m:?})"),
        None => format!("{B}::Owner::None"),
    }
}

fn expand_op(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let opts = parse_opts(attr)?;
    opts.check(FN_OPTS)?;
    let toks: Vec<TokenTree> = item.into_iter().collect();
    let sig = parse_fn(&toks)?;
    if sig.receiver.is_some() {
        return Err((
            sig.name_span,
            "a free fn has no receiver (use #[methods] for members)".into(),
        ));
    }
    let spec = Spec {
        sig: &sig,
        role: Role::Function,
        opts: &opts,
        self_ty: None,
        call_path: format!("super::{}", sig.name),
        owner: owner_of(&opts),
        desc: "DESC".into(),
        op: "Op".into(),
        scalar: "__scalar".into(),
    };
    let items = gen::gen(&spec)?;
    let module = format!(
        "#[doc(hidden)]\n#[allow(non_snake_case, non_upper_case_globals, unused_imports, unused_mut, unused_variables, clippy::all)]\n\
         {} mod {} {{\nuse super::*;\n{items}}}\n",
        sig.vis, sig.name
    );
    let mut out = strip_fn(&toks, &|_| true);
    out.extend(parse_ts(&module, sig.name_span)?);
    Ok(out)
}

/// The accessors (`name`, or `0`, `1`, .. for a tuple struct) of the fields of the struct whose
/// body follows its name in `toks`, each with the field's `#[cfg(..)]` attributes so code that
/// touches the field is compiled under the same conditions.
fn field_accessors(toks: &[TokenTree]) -> Vec<(String, String)> {
    let Some(TokenTree::Group(g)) = toks.first() else {
        return Vec::new();
    };
    let inner: Vec<TokenTree> = g.stream().into_iter().collect();
    match g.delimiter() {
        Delimiter::Brace => split_commas(&inner)
            .iter()
            .filter_map(|field| {
                let (attrs, mut k) = take_attrs(field, 0);
                let cfgs: String = attrs
                    .iter()
                    .filter(|a| a.path == "cfg")
                    .filter_map(|a| a.args.as_ref().map(|g| format!("#[cfg{g}]")))
                    .collect();
                if field.get(k).is_some_and(|t| is_ident(t, "pub")) {
                    k += 1;
                    if matches!(field.get(k), Some(TokenTree::Group(p)) if p.delimiter() == Delimiter::Parenthesis) {
                        k += 1;
                    }
                }
                match (field.get(k), field.get(k + 1)) {
                    (Some(TokenTree::Ident(name)), Some(c)) if is_punct(c, ':') => {
                        Some((cfgs, name.to_string()))
                    }
                    _ => None,
                }
            })
            .collect(),
        Delimiter::Parenthesis => (0..split_commas(&inner).len())
            .map(|n| (String::new(), n.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn expand_class(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let opts = parse_opts(attr)?;
    opts.check(&[
        "name", "rename", "only", "skip", "module", "generic", "hint", "extends",
    ])?;
    let toks: Vec<TokenTree> = item.clone().into_iter().collect();
    let doc = doc_of(&toks);
    let (_, mut i) = take_attrs(&toks, 0);
    while i < toks.len() && !is_ident(&toks[i], "struct") {
        i += 1;
    }
    let Some(TokenTree::Ident(name)) = toks.get(i + 1) else {
        return Err((Span::call_site(), "expected a struct".into()));
    };
    if toks.get(i + 2).is_some_and(|t| is_punct(t, '<')) {
        return Err((name.span(), "generic classes are not supported".into()));
    }
    let ty = name.to_string();
    let module = opts.get("module").or(opts.get("__module"));
    let flags = if opts.has("generic") {
        format!("{B}::CLASS_GENERIC")
    } else {
        "0".into()
    };
    let doc = match &doc {
        Some(d) => format!("::core::option::Option::Some({d:?})"),
        None => "::core::option::Option::None".into(),
    };
    let module = match module {
        Some(m) => format!("::core::option::Option::Some({m:?})"),
        None => "::core::option::Option::None".into(),
    };
    let fields = field_accessors(&toks[i + 2..]);
    let trace_fields: String = fields
        .iter()
        .map(|(cfg, f)| format!("{cfg}(&{B}::__private::Probe(&self.{f})).__lumen_trace(v);\n"))
        .collect();
    let clear_fields: String = fields
        .iter()
        .map(|(cfg, f)| format!("{cfg}(&mut {B}::__private::ProbeMut(&mut self.{f})).__lumen_clear();\n"))
        .collect();
    let inheritance = match opts.get("extends") {
        Some(base) => format!(
            "fn view(&self, ty: ::std::any::TypeId) -> Option<&dyn ::std::any::Any> {{ if ty == ::std::any::TypeId::of::<Self>() {{ Some(self) }} else {{ <{base} as {B}::Class>::view(&self.base, ty) }} }}
             fn view_mut(&mut self, ty: ::std::any::TypeId) -> Option<&mut dyn ::std::any::Any> {{ if ty == ::std::any::TypeId::of::<Self>() {{ Some(self) }} else {{ <{base} as {B}::Class>::view_mut(&mut self.base, ty) }} }}"
        ),
        None => String::new(),
    };
    let base_impl = match opts.get("extends") {
        Some(base) => format!("impl<H: {B}::Host> {B}::Inheritance<H> for {ty} where {base}: {B}::Methods<H> {{ fn base_class(ctx: &mut H::Ctx) -> Result<Option<H::Value>, H::Error> {{ H::class_object::<{base}>(ctx).map(Some) }} }}"),
        None => format!("impl<H: {B}::Host> {B}::Inheritance<H> for {ty} {{ fn base_class(_: &mut H::Ctx) -> Result<Option<H::Value>, H::Error> {{ Ok(None) }} }}"),
    };
    let code = format!(
        "impl {B}::Class for {ty} {{\n\
           const DESC: &'static {B}::ClassDesc = &{B}::ClassDesc {{ name: {ty:?}, {}, module: {module}, doc: {doc}, flags: {flags} }};\n\
           #[allow(unused_imports, unused_variables)]\n\
           fn gc_trace(&self, v: &mut dyn {B}::Visit) {{\n\
             use {B}::__private::{{ViaNone as _, ViaTrace as _}};\n\
             {trace_fields}\
           }}\n\
           #[allow(unused_imports, unused_variables)]\n\
           fn gc_clear(&mut self) {{\n\
             use {B}::__private::{{ViaNoneMut as _, ViaTraceMut as _}};\n\
             {clear_fields}\
           }}\n\
         {inheritance}}}\n{base_impl}\n\
         impl {B}::Elem for {ty} {{}}\n\
         impl<H: {B}::Host> {B}::IntoRet<H> for {ty} {{\n\
           const MAY_RUN: bool = false;\n\
           #[inline]\n\
           fn into_ret(self, ctx: &mut H::Ctx) -> ::core::result::Result<H::Value, H::Error> {{ H::new_instance(ctx, self) }}\n\
         }}\n",
        names_fields(&opts)
    );
    let mut out = item;
    out.extend(parse_ts(&code, name.span())?);
    Ok(out)
}

/// Split a block body into items (attributes included). An item ends at its first top-level
/// `;` or brace group (`const` / `static` / `use` / `type` items only at `;`).
fn split_items(toks: &[TokenTree]) -> Vec<Vec<TokenTree>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        let (_, after_attrs) = take_attrs(toks, i);
        let semi_item = toks[after_attrs..].iter().take(4).any(|t| {
            ["const", "static", "use", "type"]
                .iter()
                .any(|k| is_ident(t, k))
        }) && !toks[after_attrs..]
            .iter()
            .take(4)
            .any(|t| is_ident(t, "fn"));
        let mut j = after_attrs;
        while j < toks.len() {
            let t = &toks[j];
            if is_punct(t, ';') {
                j += 1;
                break;
            }
            if !semi_item && matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace) {
                j += 1;
                break;
            }
            j += 1;
        }
        out.push(toks[i..j].to_vec());
        i = j;
    }
    out
}

const MEMBER_ATTRS: &[&str] = &[
    "constructor",
    "classmethod",
    "getter",
    "setter",
    "proto",
    "method",
    "skip",
];

fn expand_methods(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    if !attr.is_empty() {
        return Err((
            Span::call_site(),
            "#[methods] takes no options (put them on #[class])".into(),
        ));
    }
    let toks: Vec<TokenTree> = item.into_iter().collect();
    let (_, mut i) = take_attrs(&toks, 0);
    if !toks.get(i).is_some_and(|t| is_ident(t, "impl")) {
        return Err((Span::call_site(), "expected an impl block".into()));
    }
    i += 1;
    if toks.get(i).is_some_and(|t| is_punct(t, '<')) {
        return Err((
            toks[i].span(),
            "generic impl blocks are not supported".into(),
        ));
    }
    let body_idx = toks
        .iter()
        .position(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace))
        .ok_or((Span::call_site(), "expected an impl body".to_string()))?;
    let self_ty = type_str(&toks[i..body_idx]);
    if self_ty.contains(" for ") || self_ty.contains('<') {
        return Err((
            toks[i].span(),
            "#[methods] goes on an inherent impl of a non-generic #[class]".into(),
        ));
    }
    let TokenTree::Group(body) = &toks[body_idx] else {
        unreachable!()
    };
    let body_toks: Vec<TokenTree> = body.stream().into_iter().collect();
    let modname = format!("__lumen_bind_{self_ty}");
    let mut gen_items = String::new();
    let mut members = Vec::new();
    let mut new_body: Vec<TokenTree> = Vec::new();
    for (n, it) in split_items(&body_toks).into_iter().enumerate() {
        let is_fn = {
            let (_, s) = take_attrs(&it, 0);
            it[s..].iter().take(6).any(|t| is_ident(t, "fn"))
        };
        if !is_fn {
            new_body.extend(it);
            continue;
        }
        let (attrs, _) = take_attrs(&it, 0);
        let stripped = strip_fn(&it, &|a| !MEMBER_ATTRS.contains(&a.path.as_str()));
        new_body.extend(stripped);
        if attrs.iter().any(|a| a.path == "skip") {
            continue;
        }
        let sig = parse_fn(&it)?;
        let has_this = sig
            .params
            .iter()
            .any(|p| base_name(&p.ts) == "This" && !p.ts.starts_with('&'));
        let instance = sig.receiver.is_some() || has_this;
        let mut opts = Opts::default();
        let mut role = if instance { Role::Method } else { Role::Static };
        for a in &attrs {
            match a.path.as_str() {
                "constructor" => role = Role::Constructor,
                "classmethod" => {
                    role = Role::Static;
                    opts.flags.push(("__classmethod".into(), a.span));
                }
                "getter" => role = Role::Getter,
                "setter" => role = Role::Setter,
                "proto" => {
                    let args: Vec<TokenTree> = a.args_ts().into_iter().collect();
                    let p = args.first().map(|t| t.to_string()).unwrap_or_default();
                    if !PROTOCOLS.contains(&p.as_str()) {
                        return Err((
                            a.span,
                            format!(
                                "unknown protocol `{p}` (expected one of: {})",
                                PROTOCOLS.join(", ")
                            ),
                        ));
                    }
                    role = Role::Proto(p);
                    if args.get(1).is_some_and(|t| is_punct(t, ',')) {
                        opts.extend(parse_opts(args[2..].iter().cloned().collect())?);
                    }
                    continue;
                }
                "method" => {}
                _ => continue,
            }
            opts.extend(parse_opts(a.args_ts())?);
        }
        opts.check(FN_OPTS)?;
        let spec = Spec {
            sig: &sig,
            role,
            opts: &opts,
            self_ty: Some(&self_ty),
            call_path: format!("<{self_ty}>::{}", sig.name),
            owner: format!("{B}::Owner::Class(<{self_ty} as {B}::Class>::DESC)"),
            desc: format!("D{n}"),
            op: format!("M{n}"),
            scalar: format!("__scalar{n}"),
        };
        gen_items.push_str(&gen::gen(&spec)?);
        members.push(format!("M{n}"));
    }
    let mut out: Vec<TokenTree> = toks[..body_idx].to_vec();
    let mut g = proc_macro::Group::new(Delimiter::Brace, new_body.into_iter().collect());
    g.set_span(body.span());
    out.push(TokenTree::Group(g));
    out.extend(toks[body_idx + 1..].iter().cloned());
    let bounds = members
        .iter()
        .map(|m| format!("{modname}::{m}: {B}::Native<H>"))
        .collect::<Vec<_>>()
        .join(", ");
    let pushes = members
        .iter()
        .map(|m| format!("out.push({B}::FnItem::of::<{modname}::{m}>());"))
        .collect::<Vec<_>>()
        .join("\n");
    let code = format!(
        "#[doc(hidden)]\n#[allow(non_snake_case, non_upper_case_globals, unused_imports, unused_mut, unused_variables, clippy::all)]\n\
         mod {modname} {{\nuse super::*;\n{gen_items}}}\n\
         #[allow(private_bounds)]\n\
         impl<H: {B}::Host> {B}::Methods<H> for {self_ty} where {self_ty}: {B}::Inheritance<H>, {bounds} {{\n\
           fn base_class(ctx: &mut H::Ctx) -> Result<Option<H::Value>, H::Error> {{ <Self as {B}::Inheritance<H>>::base_class(ctx) }}\n\
           fn members(out: &mut ::std::vec::Vec<{B}::FnItem<H>>) {{\n{pushes}\n}}\n\
         }}\n"
    );
    let mut ts: TokenStream = out.into_iter().collect();
    ts.extend(parse_ts(&code, body.span())?);
    Ok(ts)
}

const PROTOCOLS: &[&str] = &[
    "init",
    "len",
    "getitem",
    "setitem",
    "delitem",
    "contains",
    "iter",
    "next",
    "reversed",
    "repr",
    "str",
    "hash",
    "bool",
    "eq",
    "ne",
    "lt",
    "le",
    "gt",
    "ge",
    "add",
    "radd",
    "iadd",
    "sub",
    "rsub",
    "isub",
    "mul",
    "rmul",
    "imul",
    "and",
    "rand",
    "iand",
    "or",
    "ror",
    "ior",
    "xor",
    "rxor",
    "ixor",
    "neg",
    "pos",
    "abs",
    "invert",
    "index",
    "int",
    "float",
    "call",
    "copy",
    "deepcopy",
    "reduce",
    "sizeof",
    "enter",
    "exit",
    "await",
    "aiter",
    "anext",
    "truediv",
    "rtruediv",
    "itruediv",
    "floordiv",
    "rfloordiv",
    "ifloordiv",
    "mod",
    "rmod",
    "imod",
    "pow",
    "rpow",
    "ipow",
    "lshift",
    "rlshift",
    "ilshift",
    "rshift",
    "rrshift",
    "irshift",
    "matmul",
    "rmatmul",
    "imatmul",
    "divmod",
    "rdivmod",
    "getattribute",
    "setattr",
    "delattr",
];

fn expand_module(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let opts = parse_opts(attr)?;
    opts.check(&["name", "rename"])?;
    let toks: Vec<TokenTree> = item.into_iter().collect();
    let doc = doc_of(&toks);
    let (_, mut i) = take_attrs(&toks, 0);
    while i < toks.len() && !is_ident(&toks[i], "mod") {
        i += 1;
    }
    let Some(TokenTree::Ident(mod_name)) = toks.get(i + 1) else {
        return Err((Span::call_site(), "expected `mod name { .. }`".into()));
    };
    let Some(TokenTree::Group(body)) = toks.get(i + 2) else {
        return Err((mod_name.span(), "expected an inline module body".into()));
    };
    let mname = opts
        .get("name")
        .unwrap_or(&mod_name.to_string())
        .to_string();
    let body_toks: Vec<TokenTree> = body.stream().into_iter().collect();
    let mut new_body: Vec<TokenTree> = Vec::new();
    let mut bounds: Vec<String> = Vec::new();
    let mut pushes: Vec<String> = Vec::new();
    let mut gates: Vec<String> = Vec::new();
    for it in split_items(&body_toks) {
        let (attrs, after) = take_attrs(&it, 0);
        let bind = attrs.iter().find(|a| {
            ["op", "class", "methods", "constant", "init"]
                .iter()
                .any(|n| a.is(n))
        });
        let Some(bind) = bind else {
            new_body.extend(it);
            continue;
        };
        let cfgs: Vec<String> = attrs
            .iter()
            .filter(|a| a.path == "cfg")
            .map(|a| a.args_ts().to_string())
            .collect();
        let kind = bind.name().to_string();
        let mut kept: Vec<TokenTree> = Vec::new();
        for a in &attrs {
            if !std::ptr::eq(a, bind) {
                kept.extend(a.tokens.iter().cloned());
                continue;
            }
            let args = a.args_ts().to_string();
            let sep = if args.trim().is_empty() { "" } else { ", " };
            let new_attr = match kind.as_str() {
                "op" | "class" => format!("#[{B}::{kind}(__module = {mname:?}{sep}{args})]"),
                "methods" => format!("#[{B}::methods({args})]"),
                _ => String::new(),
            };
            kept.extend(parse_ts(&new_attr, a.span)?);
        }
        let rest = &it[after..];
        let ident_after = |kw: &str| -> Option<String> {
            let k = rest.iter().position(|t| is_ident(t, kw))?;
            match rest.get(k + 1) {
                Some(TokenTree::Ident(id)) => Some(id.to_string()),
                _ => None,
            }
        };
        let mut ibounds: Vec<String> = Vec::new();
        let mut ipushes: Vec<String> = Vec::new();
        match kind.as_str() {
            "op" => {
                let name = ident_after("fn").ok_or((bind.span, "expected a fn".to_string()))?;
                ibounds.push(format!("{name}::Op: {B}::Native<H>"));
                ipushes.push(format!(
                    "out.functions.push({B}::FnItem::of::<{name}::Op>());"
                ));
            }
            "class" => {
                let name =
                    ident_after("struct").ok_or((bind.span, "expected a struct".to_string()))?;
                ibounds.push(format!("{name}: {B}::Methods<H>"));
                ipushes.push(format!(
                    "out.classes.push({B}::ClassItem {{ desc: <{name} as {B}::Class>::DESC, object: <H as {B}::Host>::class_object::<{name}> }});"
                ));
            }
            "constant" => {
                let name =
                    ident_after("const").ok_or((bind.span, "expected a `const`".to_string()))?;
                let k = rest.iter().position(|t| is_ident(t, "const")).unwrap();
                let eq = rest
                    .iter()
                    .position(|t| is_punct(t, '='))
                    .ok_or((bind.span, "expected `const X: T = ..;`".to_string()))?;
                let ty = render(&rest[k + 3..eq], None, Lt::Infer);
                let o = parse_opts(bind.args_ts())?;
                o.check(&["name"])?;
                let shown = o.get("name").unwrap_or(&name).to_string();
                ibounds.push(format!("{ty}: {B}::IntoRet<H>"));
                ipushes.push(format!(
                    "out.constants.push({B}::ConstItem {{ name: {shown:?}, value: |ctx| {B}::__private::constant::<H, {ty}>(ctx, {name}) }});"
                ));
            }
            "init" => {
                let sig = parse_fn(rest)?;
                if sig.params.len() != 2 {
                    return Err((
                        sig.name_span,
                        "#[init] fn takes `(ctx: &mut Ctx, module: &Value)`".into(),
                    ));
                }
                let ctx = render(&sig.params[0].ty[2..], None, Lt::Infer);
                let val = render(&sig.params[1].ty[1..], None, Lt::Infer);
                let ret = sig
                    .ret
                    .as_ref()
                    .map(|r| render(r, None, Lt::Infer))
                    .unwrap_or_else(|| "()".into());
                ibounds.push(format!("H: {B}::Host<Ctx = {ctx}, Value = {val}>"));
                ibounds.push(format!("{ret}: {B}::IntoRet<H>"));
                ipushes.push(format!(
                    "out.init = ::core::option::Option::Some(|ctx, m| {{ let r = {}(ctx, m); {B}::IntoRet::<H>::into_ret(r, ctx).map(|_| ()) }});",
                    sig.name
                ));
            }
            _ => {}
        }
        if cfgs.is_empty() {
            bounds.extend(ibounds);
            pushes.extend(ipushes);
        } else {
            // A where-clause cannot be cfg-gated, so a gated item registers through a per-item trait
            // whose impl is gated instead.
            let gate = format!("__BindGate{}", gates.len());
            let pred = if cfgs.len() == 1 {
                cfgs[0].clone()
            } else {
                format!("all({})", cfgs.join(", "))
            };
            gates.push(format!(
                "trait {gate}<H: {B}::Host> {{ fn push(out: &mut {B}::ModuleItems<H>); }}\n\
                 #[cfg({pred})]\n\
                 #[allow(private_bounds, clippy::all)]\n\
                 impl<H: {B}::Host> {gate}<H> for () where {} {{ fn push(out: &mut {B}::ModuleItems<H>) {{ {} }} }}\n\
                 #[cfg(not({pred}))]\n\
                 impl<H: {B}::Host> {gate}<H> for () {{ fn push(_: &mut {B}::ModuleItems<H>) {{}} }}\n",
                ibounds.join(", "),
                ipushes.join("\n")
            ));
            bounds.push(format!("(): {gate}<H>"));
            pushes.push(format!("<() as {gate}<H>>::push(out);"));
        }
        new_body.extend(kept);
        new_body.extend(rest.iter().cloned());
    }
    let doc = match &doc {
        Some(d) => format!("::core::option::Option::Some({d:?})"),
        None => "::core::option::Option::None".into(),
    };
    let renames = opts
        .map("rename")
        .iter()
        .map(|(h, n)| format!("({h:?}, {n:?})"))
        .collect::<Vec<_>>()
        .join(", ");
    let explicit = match opts.get("name") {
        Some(n) => format!("::core::option::Option::Some({n:?})"),
        None => "::core::option::Option::None".into(),
    };
    let code = format!(
        "/// The module declaration (`Host::module_object::<Module>`).\n\
         pub struct Module;\n\
         static __LUMEN_BIND_MODULE: {B}::ModuleDesc = {B}::ModuleDesc {{ name: {:?}, explicit: {explicit}, renames: &[{renames}], doc: {doc} }};\n\
         #[allow(private_bounds, clippy::all)]\n\
         impl<H: {B}::Host> {B}::Module<H> for Module where {} {{\n\
           const DESC: &'static {B}::ModuleDesc = &__LUMEN_BIND_MODULE;\n\
           fn items(out: &mut {B}::ModuleItems<H>) {{\n{}\n}}\n\
         }}\n",
        mod_name.to_string(),
        bounds.join(",\n"),
        pushes.join("\n")
    );
    new_body.extend(parse_ts(&code, mod_name.span())?);
    new_body.extend(parse_ts(&gates.join("\n"), mod_name.span())?);
    let mut out: Vec<TokenTree> = toks[..i + 2].to_vec();
    let mut g = proc_macro::Group::new(Delimiter::Brace, new_body.into_iter().collect());
    g.set_span(body.span());
    out.push(TokenTree::Group(g));
    Ok(out.into_iter().collect())
}
