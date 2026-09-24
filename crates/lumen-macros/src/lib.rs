//! Binding macros for lumen. Use them through `lumen` with the `macros` feature:
//!
//! ```ignore
//! use lumen::embed::{Ctx, OpError, Value};
//!
//! #[lumen::op]
//! fn aes_ecb(encrypt: bool, key: &[u8], data: &[u8]) -> Result<Vec<u8>, OpError> { .. }
//!
//! #[lumen::op(fast)]
//! fn clamp(x: f64, lo: f64, hi: f64) -> f64 { x.max(lo).min(hi) }
//!
//! engine.define_ops("crypto", lumen::ops![aes_ecb, clamp]);
//! engine.define_op(&clamp::DESC);
//! ```
//!
//! `#[op]` keeps the fn as written and adds a module of the same name (`clamp::DESC`, a
//! `lumen::embed::OpDesc`) holding the generated `NativeFn` wrapper. Ops must be module-level
//! items (the wrapper calls `super::<name>`).
//!
//! Options: `name = "jsName"`, `fast` (also emit an unboxed `extern "C"` entry — only
//! `f64`/`i32`/`u32`/`bool` parameters and result), `coerce` (JS ToNumber/ToString/ToBoolean
//! instead of strict type checks), `async` (the fn runs off the JS thread, on the host's worker
//! pool, and the op returns a promise of its result: arguments are converted on the JS thread —
//! so they must be owned `Send` types such as `Vec<u8>`, `String`, numbers — and moved to the
//! pool; the result (`IntoJs + Send`) is converted back on the JS thread when the event loop
//! picks up the completion; an `Err`, or an argument error, rejects. Without a host event loop
//! the fn runs inline and the promise is already settled. See `lumen::embed::AsyncHost`).
//!
//! Parameters are JS arguments converted with `lumen::embed::FromJs`, except these injected
//! ones: `&mut Ctx` (the interpreter; also `&mut Interp`), `This<T>` (the receiver), and
//! `&mut State<T>` / `&State<T>` (the host state slot `T`). `length` counts the JS parameters
//! before the first `Option<_>` one.
//!
//! `#[class]` on a struct and `#[methods]` on its (single) inherent impl expose it as a JS class;
//! see the lumen `embed` docs.

use proc_macro::{Delimiter, Spacing, Span, TokenStream, TokenTree};

type Res<T> = Result<T, (Span, String)>;

/// Expose a Rust fn as a JS function (see the crate docs).
#[proc_macro_attribute]
pub fn op(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_op(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[op]: {msg}")),
    }
}

/// Expose a struct as a JS class; pair it with `#[methods]` on its impl block.
/// Option: `name = "JsName"` (default: the struct name).
#[proc_macro_attribute]
pub fn class(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_class(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[class]: {msg}")),
    }
}

/// The JS members of a `#[class]` struct. Every fn in the block is exported (unless marked
/// `#[skip]`): `&self` / `&mut self` fns become prototype methods, fns without a receiver
/// static methods. Member attributes: `#[constructor]`, `#[getter]`, `#[setter]`,
/// `#[method]`, `#[skip]`; each takes the `#[op]` options (`name = ".."`, `coerce`, `async`).
/// JS names default to the camelCase of the Rust name (a setter drops a `set_` prefix).
#[proc_macro_attribute]
pub fn methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    match expand_methods(attr, item.clone()) {
        Ok(ts) => ts,
        Err((span, msg)) => with_error(item, span, &format!("#[methods]: {msg}")),
    }
}

// ---- errors -----------------------------------------------------------------------------------

fn compile_error(span: Span, msg: &str) -> TokenStream {
    let mut ts: TokenStream = format!("::core::compile_error!{{ {msg:?} }}").parse().unwrap();
    ts = ts
        .into_iter()
        .map(|mut t| {
            t.set_span(span);
            t
        })
        .collect();
    ts
}

/// Keep the item (so its own errors and uses still resolve) and add the macro error.
fn with_error(item: TokenStream, span: Span, msg: &str) -> TokenStream {
    let mut out = item;
    out.extend(compile_error(span, msg));
    out
}

// ---- attribute arguments ----------------------------------------------------------------------

#[derive(Default)]
struct Args {
    flags: Vec<(String, Span)>,
    kv: Vec<(String, String, Span)>,
}

impl Args {
    fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == f)
    }
    fn get(&self, k: &str) -> Option<&str> {
        self.kv.iter().find(|(a, _, _)| a == k).map(|(_, v, _)| v.as_str())
    }
    fn check(&self, allowed: &[&str]) -> Res<()> {
        for (f, s) in &self.flags {
            if !allowed.contains(&f.as_str()) {
                return Err((*s, format!("unknown option `{f}`")));
            }
        }
        for (k, _, s) in &self.kv {
            if !allowed.contains(&k.as_str()) {
                return Err((*s, format!("unknown option `{k}`")));
            }
        }
        Ok(())
    }
}

fn parse_args(ts: TokenStream) -> Res<Args> {
    let mut args = Args::default();
    let toks: Vec<TokenTree> = ts.into_iter().collect();
    for part in split_commas(&toks) {
        match part.as_slice() {
            [] => {}
            [TokenTree::Ident(i)] => args.flags.push((i.to_string(), i.span())),
            [TokenTree::Ident(i), TokenTree::Punct(p), TokenTree::Literal(l)] if p.as_char() == '=' => {
                args.kv.push((i.to_string(), string_lit(l)?, i.span()))
            }
            other => {
                return Err((
                    other[0].span(),
                    "expected `flag` or `key = \"value\"`".to_string(),
                ))
            }
        }
    }
    Ok(args)
}

fn string_lit(l: &proc_macro::Literal) -> Res<String> {
    let s = l.to_string();
    match s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(body) if !body.contains('\\') => Ok(body.to_string()),
        _ => Err((l.span(), "expected a plain string literal".to_string())),
    }
}

/// Split at top-level commas (angle-bracket depth 0; `->` is not a closing bracket).
fn split_commas(toks: &[TokenTree]) -> Vec<Vec<TokenTree>> {
    let mut out = vec![Vec::new()];
    let mut depth = 0i32;
    let mut prev_dash = false;
    for t in toks {
        if let TokenTree::Punct(p) = t {
            match p.as_char() {
                '<' => depth += 1,
                '>' if !prev_dash => depth -= 1,
                ',' if depth == 0 => {
                    out.push(Vec::new());
                    prev_dash = false;
                    continue;
                }
                _ => {}
            }
            prev_dash = p.as_char() == '-' && p.spacing() == Spacing::Joint;
        } else {
            prev_dash = false;
        }
        out.last_mut().unwrap().push(t.clone());
    }
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

// ---- type strings -----------------------------------------------------------------------------

/// A compact, lifetime-free rendering of a type (`&'a mut [u8]` -> `&mut [u8]`).
fn type_str(toks: &[TokenTree]) -> String {
    let mut out = String::new();
    let mut prev_word = false;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            TokenTree::Punct(p) if p.as_char() == '\'' => {
                i += 2; // lifetime: `'` + ident
                continue;
            }
            TokenTree::Punct(p) => {
                out.push(p.as_char());
                prev_word = false;
            }
            TokenTree::Ident(id) => {
                if prev_word {
                    out.push(' ');
                }
                out.push_str(&id.to_string());
                prev_word = true;
            }
            TokenTree::Literal(l) => {
                if prev_word {
                    out.push(' ');
                }
                out.push_str(&l.to_string());
                prev_word = true;
            }
            TokenTree::Group(g) => {
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                let (o, c) = match g.delimiter() {
                    Delimiter::Parenthesis => ("(", ")"),
                    Delimiter::Bracket => ("[", "]"),
                    Delimiter::Brace => ("{", "}"),
                    Delimiter::None => ("", ""),
                };
                out.push_str(o);
                out.push_str(&type_str(&inner));
                out.push_str(c);
                prev_word = false;
            }
        }
        i += 1;
    }
    out
}

/// `a::b::Name<..>` -> `Name`.
fn base_name(ty: &str) -> &str {
    let base = ty.split('<').next().unwrap_or(ty);
    base.rsplit("::").next().unwrap_or(base).trim()
}

// ---- fn signatures ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Receiver {
    Ref,
    Mut,
}

#[derive(Clone, PartialEq, Eq)]
enum Kind {
    /// A JS argument at this index.
    Js(u32),
    Ctx,
    This,
    State { mutable: bool },
}

struct Param {
    name: String,
    ty: String,
    kind: Kind,
    span: Span,
}

struct Sig {
    name: String,
    name_span: Span,
    is_async: bool,
    receiver: Option<Receiver>,
    params: Vec<Param>,
    ret: Option<String>,
}

fn is_punct(t: &TokenTree, c: char) -> bool {
    matches!(t, TokenTree::Punct(p) if p.as_char() == c)
}
fn is_ident(t: &TokenTree, s: &str) -> bool {
    matches!(t, TokenTree::Ident(i) if i.to_string() == s)
}

/// Skip outer attributes starting at `i`, returning them.
fn take_attrs(toks: &[TokenTree], mut i: usize) -> (Vec<(usize, usize)>, usize) {
    let mut attrs = Vec::new();
    while i + 1 < toks.len() && is_punct(&toks[i], '#') {
        match &toks[i + 1] {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => {
                attrs.push((i, i + 2));
                i += 2;
            }
            _ => break,
        }
    }
    (attrs, i)
}

/// Parse a fn item (`attrs vis qualifiers fn name <generics> (params) -> ret where .. { body }`).
fn parse_fn(toks: &[TokenTree]) -> Res<Sig> {
    let (_, mut i) = take_attrs(toks, 0);
    let span0 = toks.first().map_or(Span::call_site(), |t| t.span());
    // Visibility.
    if i < toks.len() && is_ident(&toks[i], "pub") {
        i += 1;
        if matches!(toks.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis) {
            i += 1;
        }
    }
    let mut is_async = false;
    loop {
        match toks.get(i) {
            Some(t) if is_ident(t, "const") || is_ident(t, "default") => i += 1,
            Some(t) if is_ident(t, "async") => {
                is_async = true;
                i += 1;
            }
            Some(t) if is_ident(t, "unsafe") => {
                return Err((t.span(), "unsafe fns cannot be bound".into()))
            }
            Some(t) if is_ident(t, "extern") => {
                return Err((t.span(), "extern fns cannot be bound".into()))
            }
            _ => break,
        }
    }
    if !toks.get(i).is_some_and(|t| is_ident(t, "fn")) {
        return Err((span0, "expected a fn".into()));
    }
    i += 1;
    let Some(TokenTree::Ident(name)) = toks.get(i) else {
        return Err((span0, "expected a fn name".into()));
    };
    let name_span = name.span();
    let name = name.to_string();
    i += 1;
    // Generics: lifetimes only.
    if toks.get(i).is_some_and(|t| is_punct(t, '<')) {
        let mut depth = 0;
        let mut prev_dash = false;
        let mut prev_quote = false;
        loop {
            let Some(t) = toks.get(i) else {
                return Err((name_span, "unterminated generics".into()));
            };
            match t {
                TokenTree::Punct(p) => {
                    match p.as_char() {
                        '<' => depth += 1,
                        '>' if !prev_dash => depth -= 1,
                        _ => {}
                    }
                    prev_dash = p.as_char() == '-';
                    prev_quote = p.as_char() == '\'';
                }
                TokenTree::Ident(id) if depth == 1 && !prev_quote => {
                    return Err((
                        id.span(),
                        "generic type/const parameters are not supported (lifetimes only)".into(),
                    ));
                }
                _ => {
                    prev_dash = false;
                    prev_quote = false;
                }
            }
            i += 1;
            if depth == 0 {
                break;
            }
        }
    }
    let Some(TokenTree::Group(pg)) = toks.get(i) else {
        return Err((name_span, "expected a parameter list".into()));
    };
    if pg.delimiter() != Delimiter::Parenthesis {
        return Err((pg.span(), "expected a parameter list".into()));
    }
    i += 1;
    let ptoks: Vec<TokenTree> = pg.stream().into_iter().collect();
    let mut receiver = None;
    let mut params = Vec::new();
    let mut js = 0u32;
    for (n, raw) in split_commas(&ptoks).into_iter().enumerate() {
        let (_, start) = take_attrs(&raw, 0);
        let p = &raw[start..];
        let colon = (0..p.len()).find(|&k| {
            matches!(&p[k], TokenTree::Punct(c) if c.as_char() == ':' && c.spacing() == Spacing::Alone)
                && !(k > 0 && matches!(&p[k - 1], TokenTree::Punct(c) if c.as_char() == ':' && c.spacing() == Spacing::Joint))
        });
        let span = p.first().map_or(pg.span(), |t| t.span());
        let Some(colon) = colon else {
            // A receiver.
            if n != 0 || !p.iter().any(|t| is_ident(t, "self")) {
                return Err((span, "unsupported parameter".into()));
            }
            let s = type_str(p);
            receiver = Some(match s.as_str() {
                "&self" => Receiver::Ref,
                "&mut self" => Receiver::Mut,
                _ => {
                    return Err((
                        span,
                        "the receiver must be `&self` or `&mut self` (the value lives in the JS object)".into(),
                    ))
                }
            });
            continue;
        };
        let pat = &p[..colon];
        if pat.iter().any(|t| is_ident(t, "self")) {
            return Err((span, "typed `self` receivers are not supported".into()));
        }
        let pname = match pat {
            [TokenTree::Ident(id)] => id.to_string(),
            [TokenTree::Ident(m), TokenTree::Ident(id)] if m.to_string() == "mut" => id.to_string(),
            _ => "arg".to_string(),
        };
        let ty = type_str(&p[colon + 1..]);
        let base = base_name(ty.trim_start_matches('&').trim_start_matches("mut "));
        let kind = if (ty.starts_with("&mut ") && !ty.contains('<'))
            && (base == "Ctx" || base == "Interp")
        {
            Kind::Ctx
        } else if ty.starts_with('&') && base == "State" {
            Kind::State {
                mutable: ty.starts_with("&mut "),
            }
        } else if base_name(&ty) == "This" && !ty.starts_with('&') {
            Kind::This
        } else {
            js += 1;
            Kind::Js(js - 1)
        };
        params.push(Param {
            name: pname,
            ty,
            kind,
            span,
        });
    }
    // Return type.
    let mut ret = None;
    if toks.get(i).is_some_and(|t| is_punct(t, '-')) && toks.get(i + 1).is_some_and(|t| is_punct(t, '>')) {
        i += 2;
        let start = i;
        while i < toks.len()
            && !is_ident(&toks[i], "where")
            && !matches!(&toks[i], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace)
            && !is_punct(&toks[i], ';')
        {
            i += 1;
        }
        ret = Some(type_str(&toks[start..i]));
    }
    Ok(Sig {
        name,
        name_span,
        is_async,
        receiver,
        params,
        ret,
    })
}

fn camel_case(s: &str) -> String {
    let mut out = String::new();
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

// ---- wrapper generation -------------------------------------------------------------------------

const P: &str = "::lumen::embed";

struct WrapperSpec<'a> {
    sig: &'a Sig,
    /// Path of the Rust fn to call (`super::name`, `<Ty>::name`).
    call_path: String,
    /// The class self type (for receivers / constructors).
    self_ty: Option<&'a str>,
    ctor: bool,
    coerce: bool,
    is_async: bool,
    /// Name of the OpDesc static this wrapper reports errors through.
    desc_ident: String,
}

/// `(flags, arity, params-literal)` for an OpDesc, plus the wrapper fn body.
fn gen_wrapper(w: &WrapperSpec, fn_name: &str) -> Res<(u32, u32, String, String)> {
    let sig = w.sig;
    let has_ctx = sig.params.iter().any(|p| p.kind == Kind::Ctx);
    let states = sig.params.iter().filter(|p| matches!(p.kind, Kind::State { .. })).count();
    if states > 1 {
        return Err((sig.name_span, "at most one `State<T>` parameter".into()));
    }
    if states == 1 && has_ctx {
        return Err((
            sig.name_span,
            "`&mut Ctx` and `State<T>` both borrow the interpreter; take `&mut Ctx` and use `ctx.op_state()`".into(),
        ));
    }
    if sig.params.iter().filter(|p| p.kind == Kind::Ctx).count() > 1 {
        return Err((sig.name_span, "at most one `&mut Ctx` parameter".into()));
    }
    if sig.is_async {
        return Err((
            sig.name_span,
            "`async fn` is not supported yet: use `#[op(async)]` on a sync fn, or return `Promise<T>` settled from the host's event loop via `Deferred`".into(),
        ));
    }
    if w.is_async {
        // The body runs on a worker thread: nothing tied to the JS thread may cross.
        if sig.receiver.is_some() || has_ctx || states == 1 {
            return Err((
                sig.name_span,
                "`async` ops run on a worker thread, so they cannot take a receiver, `&mut Ctx` or `State<T>`; do the JS-thread part in a sync op and return `ctx.spawn_blocking(..)` (a `Promise<T>`)".into(),
            ));
        }
        for p in &sig.params {
            if p.kind == Kind::This || p.ty.contains('&') || base_name(&p.ty) == "Cow" {
                return Err((
                    p.span,
                    "`async` ops run on a worker thread: take owned `Send` arguments (`Vec<u8>`, `String`, numbers, ...), not borrows or `This`".into(),
                ));
            }
        }
    }
    let mut flags = 0;
    if w.coerce {
        flags |= 1;
    }
    if has_ctx {
        flags |= 2;
    }
    if w.is_async {
        flags |= 4;
    }
    // JS parameter names and `length`.
    let js: Vec<&Param> = sig.params.iter().filter(|p| matches!(p.kind, Kind::Js(_))).collect();
    let names = js
        .iter()
        .map(|p| format!("{:?}", p.name))
        .collect::<Vec<_>>()
        .join(", ");
    let arity = js
        .iter()
        .take_while(|p| base_name(&p.ty) != "Option")
        .count() as u32;

    let mut body = String::new();
    body.push_str(&format!(
        "let __cx = {P}::__private::ArgCx::new(__ctx, &__this, __args, &{}, {flags});\n",
        w.desc_ident
    ));
    if w.ctor {
        body.push_str("let __nt = __cx.new_target()?;\n");
    }
    let mut call_args = Vec::new();
    if let Some(r) = sig.receiver {
        let ty = w.self_ty.expect("receiver outside an impl");
        let f = if r == Receiver::Mut { "class_mut" } else { "class_ref" };
        body.push_str(&format!(
            "let __self = {P}::__private::{f}::<{ty}>(&__cx, __cx.this(), {P}::Slot::THIS)?;\n"
        ));
        call_args.push("__self".to_string());
    }
    // Byte-slice parameters convert last (nothing that may run JS follows a pointer borrow).
    let deferred = |p: &Param| p.ty.contains("[u8]");
    for pass in [false, true] {
        for (k, p) in sig.params.iter().enumerate() {
            if deferred(p) != pass {
                continue;
            }
            match p.kind {
                Kind::Js(j) => body.push_str(&format!("let __a{k} = __cx.arg({j})?;\n")),
                Kind::This => body.push_str(&format!("let __a{k} = {P}::This(__cx.this_arg()?);\n")),
                _ => {}
            }
        }
    }
    for (k, p) in sig.params.iter().enumerate() {
        call_args.push(match p.kind {
            Kind::Ctx => "__c".to_string(),
            Kind::State { mutable: true } => "__s".to_string(),
            Kind::State { mutable: false } => "&*__s".to_string(),
            _ => format!("__a{k}"),
        });
    }
    let call = format!("{}({})", w.call_path, call_args.join(", "));
    let call = if w.is_async {
        format!("__cx.spawn_blocking(move || {call})?")
    } else if has_ctx {
        format!("__cx.with_ctx(|__c| {call})")
    } else if states == 1 {
        format!("__cx.with_state(|__s| {call})?")
    } else {
        call
    };
    body.push_str(&format!("let __r = {call};\n"));
    if w.ctor {
        let ty = w.self_ty.unwrap();
        body.push_str(&format!("__cx.construct_ret::<{ty}, _>(__nt, __r)\n"));
    } else if w.is_async {
        body.push_str("::core::result::Result::Ok(__r)\n");
    } else {
        body.push_str("__cx.ret(__r)\n");
    }
    let sig_s = format!(
        "(__ctx: &mut {P}::Ctx, __this: {P}::Value, __args: &[{P}::Value]) -> ::core::result::Result<{P}::Value, {P}::Value>"
    );
    let func = if w.is_async {
        format!(
            "fn {fn_name}_inner{sig_s} {{\n{body}}}\n\
             fn {fn_name}{sig_s} {{\n let __r = {fn_name}_inner(&mut *__ctx, __this, __args);\n {P}::__private::async_ret(__ctx, __r)\n}}\n"
        )
    } else {
        format!("fn {fn_name}{sig_s} {{\n{body}}}\n")
    };
    Ok((flags, arity, names, func))
}

fn fast_kind(ty: &str) -> Option<&'static str> {
    Some(match ty {
        "f64" => "F64",
        "i32" => "I32",
        "u32" => "U32",
        "bool" => "Bool",
        _ => return None,
    })
}

// ---- #[op] --------------------------------------------------------------------------------------

fn expand_op(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let args = parse_args(attr)?;
    args.check(&["name", "fast", "coerce", "async"])?;
    let toks: Vec<TokenTree> = item.clone().into_iter().collect();
    let sig = parse_fn(&toks)?;
    if sig.receiver.is_some() {
        return Err((sig.name_span, "use #[methods] for fns with a receiver".into()));
    }
    // Visibility of the item (reused for the generated module).
    let (_, i) = take_attrs(&toks, 0);
    let mut vis = String::new();
    if toks.get(i).is_some_and(|t| is_ident(t, "pub")) {
        vis.push_str("pub");
        if let Some(TokenTree::Group(g)) = toks.get(i + 1) {
            if g.delimiter() == Delimiter::Parenthesis {
                vis.push_str(&g.to_string());
            }
        }
    }
    let js_name = args.get("name").map(str::to_string).unwrap_or_else(|| sig.name.clone());
    let w = WrapperSpec {
        sig: &sig,
        call_path: format!("super::{}", sig.name),
        self_ty: None,
        ctor: false,
        coerce: args.has("coerce"),
        is_async: args.has("async"),
        desc_ident: "DESC".into(),
    };
    let (flags, arity, names, func) = gen_wrapper(&w, "__native")?;

    let fast = if args.has("fast") {
        if args.has("async") || args.has("coerce") {
            return Err((sig.name_span, "`fast` excludes `async` and `coerce`".into()));
        }
        let mut kinds = Vec::new();
        let mut params = Vec::new();
        let mut call = Vec::new();
        for (k, p) in sig.params.iter().enumerate() {
            let fk = match (&p.kind, fast_kind(&p.ty)) {
                (Kind::Js(_), Some(fk)) => fk,
                _ => {
                    return Err((
                        p.span,
                        "fast ops take only f64 / i32 / u32 / bool JS parameters".into(),
                    ))
                }
            };
            kinds.push(format!("{P}::FastKind::{fk}"));
            if fk == "Bool" {
                params.push(format!("a{k}: i32"));
                call.push(format!("a{k} != 0"));
            } else {
                params.push(format!("a{k}: {}", p.ty));
                call.push(format!("a{k}"));
            }
        }
        let (ret_kind, ret_ty, conv) = match sig.ret.as_deref() {
            None | Some("()") => ("Void", "()", ""),
            Some(t) => match fast_kind(t) {
                Some("Bool") => ("Bool", "i32", " as i32"),
                Some(k) => (k, t, ""),
                None => {
                    return Err((
                        sig.name_span,
                        "fast ops return f64 / i32 / u32 / bool or nothing".into(),
                    ))
                }
            },
        };
        format!(
            "pub extern \"C\" fn __fast({}) -> {ret_ty} {{ super::{}({}){conv} }}\n\
             const __FAST: ::core::option::Option<{P}::FastSig> = ::core::option::Option::Some({P}::FastSig {{ \
             args: &[{}], ret: {P}::FastKind::{ret_kind}, entry: {P}::FastPtr(__fast as *const ()) }});\n",
            params.join(", "),
            sig.name,
            call.join(", "),
            kinds.join(", "),
        )
    } else {
        format!("const __FAST: ::core::option::Option<{P}::FastSig> = ::core::option::Option::None;\n")
    };

    let module = format!(
        "#[doc(hidden)]\n\
         #[allow(non_snake_case, non_upper_case_globals, unused_mut, unused_variables, clippy::all)]\n\
         {vis} mod {name} {{\n\
         pub {func}\
         {fast}\
         /// The op descriptor (register with `Engine::define_op` / `define_ops`).\n\
         pub static DESC: {P}::OpDesc = {P}::OpDesc {{ name: {js_name:?}, owner: \"\", arity: {arity}, \
         params: &[{names}], native: __native, fast: __FAST, flags: {flags} }};\n\
         }}\n",
        name = sig.name,
    );
    let mut out = item;
    out.extend(module.parse::<TokenStream>().map_err(|e| {
        (sig.name_span, format!("internal: generated code did not parse: {e}"))
    })?);
    Ok(out)
}

// ---- #[class] -----------------------------------------------------------------------------------

fn expand_class(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let args = parse_args(attr)?;
    args.check(&["name"])?;
    let toks: Vec<TokenTree> = item.clone().into_iter().collect();
    let pos = toks
        .iter()
        .position(|t| is_ident(t, "struct") || is_ident(t, "enum"))
        .ok_or((Span::call_site(), "expected a struct or enum".to_string()))?;
    let Some(TokenTree::Ident(name)) = toks.get(pos + 1) else {
        return Err((toks[pos].span(), "expected a type name".into()));
    };
    if toks.get(pos + 2).is_some_and(|t| is_punct(t, '<')) {
        return Err((name.span(), "generic classes are not supported".into()));
    }
    let ty = name.to_string();
    let js_name = args.get("name").map(str::to_string).unwrap_or_else(|| ty.clone());
    let gen = format!(
        "#[allow(clippy::all)]\n\
         impl {P}::Class for {ty} {{\n\
           const NAME: &'static str = {js_name:?};\n\
           fn class_desc() -> &'static {P}::ClassDesc {{ <{ty} as {P}::__private::ClassMethods>::desc() }}\n\
         }}\n\
         impl {P}::IntoJs for {ty} {{\n\
           const MAY_RUN_JS: bool = false;\n\
           fn into_js(self, ctx: &mut {P}::Ctx) -> ::core::result::Result<{P}::Value, {P}::Value> {{\n\
             {P}::__private::class_into_js(ctx, self)\n\
           }}\n\
         }}\n\
         impl<'a> {P}::FromJs<'a> for &'a {ty} {{\n\
           fn from_js(cx: &'a {P}::ArgCx<'_>, v: &'a {P}::Value, at: {P}::Slot) -> ::core::result::Result<Self, {P}::Value> {{\n\
             {P}::__private::class_ref(cx, v, at)\n\
           }}\n\
         }}\n\
         impl<'a> {P}::FromJs<'a> for &'a mut {ty} {{\n\
           fn from_js(cx: &'a {P}::ArgCx<'_>, v: &'a {P}::Value, at: {P}::Slot) -> ::core::result::Result<Self, {P}::Value> {{\n\
             {P}::__private::class_mut(cx, v, at)\n\
           }}\n\
         }}\n\
         impl {P}::ArrayElem for {ty} {{}}\n"
    );
    let mut out = item;
    out.extend(
        gen.parse::<TokenStream>()
            .map_err(|e| (name.span(), format!("internal: {e}")))?,
    );
    Ok(out)
}

// ---- #[methods] ---------------------------------------------------------------------------------

const MEMBER_ATTRS: &[&str] = &["constructor", "getter", "setter", "method", "skip"];

fn expand_methods(attr: TokenStream, item: TokenStream) -> Res<TokenStream> {
    let a = parse_args(attr)?;
    a.check(&[])?;
    let toks: Vec<TokenTree> = item.into_iter().collect();
    let (_, mut i) = take_attrs(&toks, 0);
    let head_start = 0;
    if !toks.get(i).is_some_and(|t| is_ident(t, "impl")) {
        return Err((
            toks.get(i).map_or(Span::call_site(), |t| t.span()),
            "expected an inherent `impl Type { .. }` block".into(),
        ));
    }
    i += 1;
    if toks.get(i).is_some_and(|t| is_punct(t, '<')) {
        return Err((toks[i].span(), "generic impls are not supported".into()));
    }
    let Some(body_pos) = toks
        .iter()
        .position(|t| matches!(t, TokenTree::Group(g) if g.delimiter() == Delimiter::Brace))
    else {
        return Err((toks[i].span(), "expected an impl body".into()));
    };
    let self_toks = &toks[i..body_pos];
    if self_toks.iter().any(|t| is_ident(t, "for")) {
        return Err((toks[i].span(), "use an inherent impl, not a trait impl".into()));
    }
    let self_ty = type_str(self_toks);
    let TokenTree::Group(body) = &toks[body_pos] else { unreachable!() };
    let items: Vec<TokenTree> = body.stream().into_iter().collect();

    // Walk the items: strip member attributes, collect fn signatures.
    let mut kept: Vec<TokenTree> = Vec::new();
    let mut members = Vec::new(); // (role, Args, Sig)
    let mut k = 0;
    while k < items.len() {
        let (attrs, after) = take_attrs(&items, k);
        let mut role: Option<(String, Args, Span)> = None;
        for &(s, e) in &attrs {
            let TokenTree::Group(g) = &items[s + 1] else { unreachable!() };
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            let name = match inner.first() {
                Some(TokenTree::Ident(id)) => id.to_string(),
                _ => String::new(),
            };
            if MEMBER_ATTRS.contains(&name.as_str()) && inner.len() <= 2 {
                if role.is_some() {
                    return Err((inner[0].span(), "one member attribute per fn".into()));
                }
                let args = match inner.get(1) {
                    Some(TokenTree::Group(ag)) if ag.delimiter() == Delimiter::Parenthesis => {
                        parse_args(ag.stream())?
                    }
                    None => Args::default(),
                    Some(t) => return Err((t.span(), "expected `(options)`".into())),
                };
                role = Some((name, args, inner[0].span()));
            } else {
                kept.extend(items[s..e].iter().cloned());
            }
        }
        // The item runs to its body (fn) or `;`.
        let mut end = after;
        let is_fn = {
            let mut j = after;
            let mut found = false;
            while j < items.len() {
                if is_ident(&items[j], "fn") {
                    found = true;
                    break;
                }
                if is_punct(&items[j], ';') || is_punct(&items[j], '=') {
                    break;
                }
                if matches!(&items[j], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace) {
                    break;
                }
                j += 1;
            }
            found
        };
        while end < items.len() {
            let stop = if is_fn {
                matches!(&items[end], TokenTree::Group(g) if g.delimiter() == Delimiter::Brace)
            } else {
                is_punct(&items[end], ';')
            };
            end += 1;
            if stop {
                break;
            }
        }
        let item_toks = &items[after..end];
        kept.extend(item_toks.iter().cloned());
        if is_fn {
            let (role_name, args, rspan) = role.unwrap_or_else(|| ("".into(), Args::default(), Span::call_site()));
            if role_name != "skip" {
                let sig = parse_fn(item_toks)?;
                members.push((role_name, args, rspan, sig));
            }
        } else if let Some((_, _, s)) = role {
            return Err((s, "member attributes go on fns".into()));
        }
        k = end;
    }

    // Generate wrappers.
    let mut gen = String::new();
    let mut member_descs = Vec::new();
    let mut ctor_desc = "::core::option::Option::None".to_string();
    for (n, (role, args, rspan, sig)) in members.iter().enumerate() {
        args.check(&["name", "coerce", "async"]).map_err(|(s, m)| (if role.is_empty() { s } else { *rspan }, m))?;
        let ctor = role == "constructor";
        let js_name = match args.get("name") {
            Some(n) => n.to_string(),
            None if role == "setter" => camel_case(sig.name.strip_prefix("set_").unwrap_or(&sig.name)),
            None => camel_case(&sig.name),
        };
        let kind = match (role.as_str(), sig.receiver.is_some()) {
            ("constructor", false) => "",
            ("constructor", true) => return Err((sig.name_span, "a constructor has no receiver".into())),
            ("getter", true) => "Getter",
            ("getter", false) => "StaticGetter",
            ("setter", true) => "Setter",
            ("setter", false) => "StaticSetter",
            (_, true) => "Method",
            (_, false) => "StaticMethod",
        };
        if ctor && args.has("async") {
            return Err((sig.name_span, "a constructor cannot be async".into()));
        }
        let js_count = sig.params.iter().filter(|p| matches!(p.kind, Kind::Js(_))).count();
        if role == "getter" && js_count != 0 {
            return Err((sig.name_span, "a getter takes no JS arguments".into()));
        }
        if role == "setter" && js_count != 1 {
            return Err((sig.name_span, "a setter takes exactly one JS argument".into()));
        }
        let fn_name = format!("__lumen_m{n}");
        let desc_ident = format!("__LUMEN_M{n}");
        let w = WrapperSpec {
            sig,
            call_path: format!("<{self_ty}>::{}", sig.name),
            self_ty: Some(&self_ty),
            ctor,
            coerce: args.has("coerce"),
            is_async: args.has("async"),
            desc_ident: desc_ident.clone(),
        };
        let (flags, arity, names, func) = gen_wrapper(&w, &fn_name)?;
        gen.push_str(&func);
        gen.push_str(&format!(
            "static {desc_ident}: {P}::OpDesc = {P}::OpDesc {{ name: {js_name:?}, \
             owner: <{self_ty} as {P}::Class>::NAME, arity: {arity}, params: &[{names}], \
             native: {fn_name}, fast: ::core::option::Option::None, flags: {flags} }};\n"
        ));
        if ctor {
            if ctor_desc != "::core::option::Option::None" {
                return Err((sig.name_span, "only one #[constructor]".into()));
            }
            ctor_desc = format!("::core::option::Option::Some(&{desc_ident})");
        } else {
            member_descs.push(format!(
                "{P}::MemberDesc {{ kind: {P}::MemberKind::{kind}, op: &{desc_ident} }}"
            ));
        }
    }
    gen.push_str(&format!(
        "static __LUMEN_CLASS: {P}::ClassDesc = {P}::ClassDesc {{ name: <{self_ty} as {P}::Class>::NAME, \
         constructor: {ctor_desc}, members: &[{}] }};\n\
         impl {P}::__private::ClassMethods for {self_ty} {{\n\
           fn desc() -> &'static {P}::ClassDesc {{ &__LUMEN_CLASS }}\n\
         }}\n",
        member_descs.join(", ")
    ));
    let wrapped = format!(
        "#[allow(non_snake_case, non_upper_case_globals, unused_mut, unused_variables, clippy::all)]\n\
         const _: () = {{\n{gen}}};\n"
    );

    // Re-emit the impl without the member attributes.
    let mut out: Vec<TokenTree> = toks[head_start..body_pos].to_vec();
    let mut g = proc_macro::Group::new(Delimiter::Brace, kept.into_iter().collect());
    g.set_span(body.span());
    out.push(TokenTree::Group(g));
    out.extend(toks[body_pos + 1..].iter().cloned());
    let mut ts: TokenStream = out.into_iter().collect();
    ts.extend(
        wrapped
            .parse::<TokenStream>()
            .map_err(|e| (Span::call_site(), format!("internal: {e}")))?,
    );
    Ok(ts)
}
