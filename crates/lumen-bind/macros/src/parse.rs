//! Token helpers, attribute options and fn-signature parsing (by hand: no dependencies).

use proc_macro::{Delimiter, Group, Spacing, Span, TokenStream, TokenTree};

pub type Res<T> = Result<T, (Span, String)>;

pub fn compile_error(span: Span, msg: &str) -> TokenStream {
    let ts: TokenStream = format!("::core::compile_error!{{ {msg:?} }}").parse().unwrap();
    ts.into_iter()
        .map(|mut t| {
            t.set_span(span);
            t
        })
        .collect()
}

/// Keep the item (so its own uses still resolve) and add the error.
pub fn with_error(item: TokenStream, span: Span, msg: &str) -> TokenStream {
    let mut out = item;
    out.extend(compile_error(span, msg));
    out
}

pub fn parse_ts(code: &str, span: Span) -> Res<TokenStream> {
    code.parse::<TokenStream>().map_err(|e| (span, format!("internal: generated code did not parse: {e}\n{code}")))
}

pub fn is_punct(t: &TokenTree, c: char) -> bool {
    matches!(t, TokenTree::Punct(p) if p.as_char() == c)
}

pub fn is_ident(t: &TokenTree, s: &str) -> bool {
    matches!(t, TokenTree::Ident(i) if i.to_string() == s)
}

/// Split at top-level commas (angle-bracket depth 0; `->` does not close a bracket).
pub fn split_commas(toks: &[TokenTree]) -> Vec<Vec<TokenTree>> {
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

/// The body of a string literal (plain with simple escapes, or raw).
pub fn string_lit(l: &proc_macro::Literal) -> Res<String> {
    let s = l.to_string();
    if let Some(raw) = s.strip_prefix('r') {
        let hashes = raw.len() - raw.trim_start_matches('#').len();
        let body = &raw[hashes..raw.len() - hashes];
        if let Some(b) = body.strip_prefix('"').and_then(|b| b.strip_suffix('"')) {
            return Ok(b.to_string());
        }
    }
    let Some(body) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) else {
        return Err((l.span(), "expected a string literal".to_string()));
    };
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some('\'') => out.push('\''),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('0') => out.push('\0'),
            Some('\n') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            _ => return Err((l.span(), "unsupported escape in string literal".to_string())),
        }
    }
    Ok(out)
}

// ---- options ----------------------------------------------------------------------------------

/// `flag`, `key = "value"`, `key(a, b)` and `key(a = "x", b = "y")` options.
#[derive(Default, Clone)]
pub struct Opts {
    pub flags: Vec<(String, Span)>,
    pub kv: Vec<(String, String, Span)>,
    pub lists: Vec<(String, Vec<String>, Span)>,
    pub maps: Vec<(String, Vec<(String, String)>, Span)>,
    /// `hint(host(key = "value", flag))`: `(host, key, value)`, passed through to the host.
    pub hints: Vec<(String, String, String)>,
    pub hint_span: Option<Span>,
}

impl Opts {
    pub fn has(&self, f: &str) -> bool {
        self.flags.iter().any(|(k, _)| k == f)
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.kv.iter().rev().find(|(a, _, _)| a == k).map(|(_, v, _)| v.as_str())
    }
    pub fn list(&self, k: &str) -> Vec<String> {
        self.lists.iter().filter(|(a, _, _)| a == k).flat_map(|(_, v, _)| v.clone()).collect()
    }
    pub fn map(&self, k: &str) -> Vec<(String, String)> {
        self.maps.iter().filter(|(a, _, _)| a == k).flat_map(|(_, v, _)| v.clone()).collect()
    }
    pub fn check(&self, allowed: &[&str]) -> Res<()> {
        let names = self
            .flags
            .iter()
            .map(|(k, s)| (k.as_str(), s))
            .chain(self.kv.iter().map(|(k, _, s)| (k.as_str(), s)))
            .chain(self.lists.iter().map(|(k, _, s)| (k.as_str(), s)))
            .chain(self.maps.iter().map(|(k, _, s)| (k.as_str(), s)))
            .chain(self.hint_span.iter().map(|s| (HINT, s)));
        for (k, s) in names {
            if !allowed.contains(&k) && !k.starts_with("__") {
                return Err((*s, format!("unknown option `{k}` (expected one of: {})", allowed.join(", "))));
            }
        }
        Ok(())
    }
    pub fn extend(&mut self, o: Opts) {
        self.flags.extend(o.flags);
        self.kv.extend(o.kv);
        self.lists.extend(o.lists);
        self.maps.extend(o.maps);
        self.hints.extend(o.hints);
        self.hint_span = self.hint_span.or(o.hint_span);
    }
}

const HINT: &str = "hint";

pub fn parse_opts(ts: TokenStream) -> Res<Opts> {
    let mut o = Opts::default();
    let toks: Vec<TokenTree> = ts.into_iter().collect();
    for part in split_commas(&toks) {
        match part.as_slice() {
            [] => {}
            [TokenTree::Ident(i)] => o.flags.push((i.to_string(), i.span())),
            [TokenTree::Ident(i), TokenTree::Punct(p), TokenTree::Literal(l)] if p.as_char() == '=' => {
                o.kv.push((i.to_string(), string_lit(l)?, i.span()))
            }
            [TokenTree::Ident(i), TokenTree::Group(g)] if g.delimiter() == Delimiter::Parenthesis && i.to_string() == "hint" => {
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                for p in split_commas(&inner) {
                    match p.as_slice() {
                        [] => {}
                        [TokenTree::Ident(h), TokenTree::Group(hg)] if hg.delimiter() == Delimiter::Parenthesis => {
                            let ho = parse_opts(hg.stream())?;
                            if !ho.lists.is_empty() || !ho.maps.is_empty() || !ho.hints.is_empty() {
                                return Err((h.span(), "hints are `flag` or `key = \"value\"`".into()));
                            }
                            for (f, _) in ho.flags {
                                o.hints.push((h.to_string(), f, String::new()));
                            }
                            for (k, v, _) in ho.kv {
                                o.hints.push((h.to_string(), k, v));
                            }
                        }
                        other => return Err((other[0].span(), "expected `host(key = \"value\", ..)`".into())),
                    }
                }
                o.hint_span = Some(i.span());
            }
            [TokenTree::Ident(i), TokenTree::Group(g)] if g.delimiter() == Delimiter::Parenthesis => {
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                if inner.iter().any(|t| is_punct(t, '=')) {
                    let mut pairs = Vec::new();
                    for p in split_commas(&inner) {
                        match p.as_slice() {
                            [TokenTree::Ident(h), TokenTree::Punct(e), TokenTree::Literal(l)] if e.as_char() == '=' => {
                                pairs.push((h.to_string(), string_lit(l)?))
                            }
                            other => return Err((other[0].span(), "expected `host = \"name\"`".into())),
                        }
                    }
                    o.maps.push((i.to_string(), pairs, i.span()));
                } else {
                    let mut items = Vec::new();
                    for p in split_commas(&inner) {
                        match p.as_slice() {
                            [TokenTree::Ident(h)] => items.push(h.to_string()),
                            other => return Err((other[0].span(), "expected a name".into())),
                        }
                    }
                    o.lists.push((i.to_string(), items, i.span()));
                }
            }
            other => return Err((other[0].span(), "expected `flag`, `key = \"value\"` or `key(..)`".into())),
        }
    }
    Ok(o)
}

// ---- attributes -------------------------------------------------------------------------------

/// An outer attribute: its path (`kw`, `lumen_bind::op`), its arguments and its tokens.
pub struct Attr {
    pub path: String,
    pub args: Option<Group>,
    pub span: Span,
    pub tokens: Vec<TokenTree>,
}

impl Attr {
    /// The last path segment (`op` for `lumen_bind::op`).
    pub fn name(&self) -> &str {
        self.path.rsplit("::").next().unwrap_or(&self.path)
    }
    /// A binding attribute named `n` (`n`, `lumen_bind::n`, `::lumen_bind::n`).
    pub fn is(&self, n: &str) -> bool {
        self.name() == n && (self.path == n || self.path.trim_start_matches("::") == format!("lumen_bind::{n}"))
    }
    pub fn args_ts(&self) -> TokenStream {
        self.args.as_ref().map(|g| g.stream()).unwrap_or_default()
    }
}

/// Outer attributes starting at `i`, and the index after them.
pub fn take_attrs(toks: &[TokenTree], mut i: usize) -> (Vec<Attr>, usize) {
    let mut attrs = Vec::new();
    while i + 1 < toks.len() && is_punct(&toks[i], '#') {
        let TokenTree::Group(g) = &toks[i + 1] else { break };
        if g.delimiter() != Delimiter::Bracket {
            break;
        }
        let inner: Vec<TokenTree> = g.stream().into_iter().collect();
        let mut path = String::new();
        let mut k = 0;
        while k < inner.len() {
            match &inner[k] {
                TokenTree::Ident(id) => path.push_str(&id.to_string()),
                TokenTree::Punct(p) if p.as_char() == ':' => path.push(':'),
                _ => break,
            }
            k += 1;
        }
        let args = match inner.get(k) {
            Some(TokenTree::Group(a)) if a.delimiter() == Delimiter::Parenthesis => Some(a.clone()),
            _ => None,
        };
        attrs.push(Attr { path, args, span: toks[i].span(), tokens: vec![toks[i].clone(), toks[i + 1].clone()] });
        i += 2;
    }
    (attrs, i)
}

/// The doc comment (`///` lines) of an attribute list.
pub fn doc_of(toks: &[TokenTree]) -> Option<String> {
    let (attrs, _) = take_attrs(toks, 0);
    let mut lines = Vec::new();
    for a in &attrs {
        if a.path != "doc" {
            continue;
        }
        let TokenTree::Group(g) = &a.tokens[1] else { continue };
        let inner: Vec<TokenTree> = g.stream().into_iter().collect();
        if let [_, TokenTree::Punct(p), TokenTree::Literal(l)] = inner.as_slice() {
            if p.as_char() == '=' {
                if let Ok(s) = string_lit(l) {
                    lines.push(s.strip_prefix(' ').unwrap_or(&s).to_string());
                }
            }
        }
    }
    if lines.is_empty() {
        None
    } else {
        // A trailing empty `///` line keeps one final newline (CPython docstrings that end in one).
        let keep_newline = lines.len() > 1 && lines.last().is_some_and(|l| l.trim().is_empty());
        let text = lines.join("\n").trim().to_string();
        Some(if keep_newline { text + "\n" } else { text })
    }
}

// ---- types ------------------------------------------------------------------------------------

/// Types that carry a lifetime parameter and may be written without it.
const LIFETIME_TYPES: &[&str] = &["Cow", "SyncFn", "KwArgs", "PyBuffer"];

/// How [`render`] treats lifetimes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lt {
    /// As written, with named lifetimes other than `'static` turned into `'_` (let bindings).
    Infer,
    /// Every elided or named lifetime is `'a` (bounds under `for<'a>`).
    Named(&'static str),
}

/// Render a type for generated code: `Self` replaced by `self_ty`, lifetimes per `lt`.
pub fn render(toks: &[TokenTree], self_ty: Option<&str>, lt: Lt) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        match t {
            TokenTree::Punct(p) if p.as_char() == '\'' => {
                let name = toks.get(i + 1).map(|t| t.to_string()).unwrap_or_default();
                let lname = match (lt, name.as_str()) {
                    (_, "static") => "'static".to_string(),
                    (Lt::Infer, _) => "'_".to_string(),
                    (Lt::Named(n), _) => n.to_string(),
                };
                out.push_str(&lname);
                out.push(' ');
                i += 2;
                continue;
            }
            TokenTree::Punct(p) if p.as_char() == '&' => {
                out.push('&');
                let next_is_lt = toks.get(i + 1).is_some_and(|t| is_punct(t, '\''));
                if let (Lt::Named(n), false) = (lt, next_is_lt) {
                    out.push_str(n);
                }
                out.push(' ');
            }
            TokenTree::Punct(p) => {
                out.push(p.as_char());
                if p.spacing() == Spacing::Alone {
                    out.push(' ');
                }
            }
            TokenTree::Ident(id) => {
                let s = id.to_string();
                if s == "Self" {
                    out.push_str(self_ty.unwrap_or("Self"));
                } else {
                    out.push_str(&s);
                }
                out.push(' ');
                if let Lt::Named(n) = lt {
                    if LIFETIME_TYPES.contains(&s.as_str()) {
                        let has_args = toks.get(i + 1).is_some_and(|t| is_punct(t, '<'));
                        if !has_args {
                            out.push_str(&format!("<{n}> "));
                        } else if !toks.get(i + 2).is_some_and(|t| is_punct(t, '\'')) {
                            out.push_str(&format!("< {n} , "));
                            i += 2;
                            continue;
                        }
                    }
                }
            }
            TokenTree::Literal(l) => {
                out.push_str(&l.to_string());
                out.push(' ');
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
                out.push_str(&render(&inner, self_ty, lt));
                out.push_str(c);
                out.push(' ');
            }
        }
        i += 1;
    }
    out.trim_end().to_string()
}

/// A compact, lifetime-free rendering for classification (`&'a mut [u8]` -> `&mut [u8]`).
pub fn type_str(toks: &[TokenTree]) -> String {
    let mut out = String::new();
    let mut prev_word = false;
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            TokenTree::Punct(p) if p.as_char() == '\'' => {
                i += 2;
                if matches!(toks.get(i), Some(TokenTree::Punct(p)) if p.as_char() == ',') {
                    i += 1;
                }
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
    out.replace("<>", "")
}

/// `a::b::Name<..>` -> `Name`.
pub fn base_name(ty: &str) -> &str {
    let ty = ty.trim_start_matches('&').trim_start_matches("mut ");
    let base = ty.split('<').next().unwrap_or(ty);
    base.rsplit("::").next().unwrap_or(base).trim()
}

/// The generic argument tokens of `Name<..>` (the first `<` to the last `>`).
pub fn generic_inner(toks: &[TokenTree]) -> Option<Vec<TokenTree>> {
    let start = toks.iter().position(|t| is_punct(t, '<'))?;
    let end = toks.iter().rposition(|t| is_punct(t, '>'))?;
    (end > start).then(|| toks[start + 1..end].to_vec())
}

// ---- fn signatures ----------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Receiver {
    Ref,
    Mut,
}

pub struct Param {
    pub name: String,
    pub ty: Vec<TokenTree>,
    /// [`type_str`] of `ty`.
    pub ts: String,
    pub attrs: Vec<Attr>,
    pub span: Span,
}

pub struct Sig {
    pub name: String,
    pub name_span: Span,
    pub vis: String,
    pub receiver: Option<Receiver>,
    pub params: Vec<Param>,
    pub ret: Option<Vec<TokenTree>>,
    pub doc: Option<String>,
}

/// Parameter attributes the macros consume.
pub const PARAM_ATTRS: &[&str] = &["kw", "kwonly", "varargs", "varkw", "default"];

/// Parse a fn item (`attrs vis qualifiers fn name <lifetimes> (params) -> ret where .. { body }`).
pub fn parse_fn(toks: &[TokenTree]) -> Res<Sig> {
    let doc = doc_of(toks);
    let (_, mut i) = take_attrs(toks, 0);
    let span0 = toks.first().map_or(Span::call_site(), |t| t.span());
    let mut vis = String::new();
    if i < toks.len() && is_ident(&toks[i], "pub") {
        vis.push_str("pub");
        i += 1;
        if let Some(TokenTree::Group(g)) = toks.get(i) {
            if g.delimiter() == Delimiter::Parenthesis {
                vis.push_str(&g.to_string());
                i += 1;
            }
        }
    }
    loop {
        match toks.get(i) {
            Some(t) if is_ident(t, "const") || is_ident(t, "default") => i += 1,
            Some(t) if is_ident(t, "async") => return Err((t.span(), "write a plain fn and add the `async` option".into())),
            Some(t) if is_ident(t, "unsafe") => return Err((t.span(), "unsafe fns cannot be bound".into())),
            Some(t) if is_ident(t, "extern") => return Err((t.span(), "extern fns cannot be bound".into())),
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
                    return Err((id.span(), "generic type parameters are not supported (lifetimes only)".into()));
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
    i += 1;
    let ptoks: Vec<TokenTree> = pg.stream().into_iter().collect();
    let mut receiver = None;
    let mut params = Vec::new();
    for (n, raw) in split_commas(&ptoks).into_iter().enumerate() {
        let (attrs, start) = take_attrs(&raw, 0);
        let p = &raw[start..];
        let span = p.first().map_or(pg.span(), |t| t.span());
        let colon = (0..p.len()).find(|&k| {
            matches!(&p[k], TokenTree::Punct(c) if c.as_char() == ':' && c.spacing() == Spacing::Alone)
                && !(k > 0 && matches!(&p[k - 1], TokenTree::Punct(c) if c.as_char() == ':' && c.spacing() == Spacing::Joint))
        });
        let Some(colon) = colon else {
            if n != 0 || !p.iter().any(|t| is_ident(t, "self")) {
                return Err((span, "unsupported parameter".into()));
            }
            receiver = Some(match type_str(p).as_str() {
                "&self" => Receiver::Ref,
                "&mut self" => Receiver::Mut,
                _ => return Err((span, "the receiver must be `&self` or `&mut self` (the value lives in the script object)".into())),
            });
            continue;
        };
        let pat = &p[..colon];
        let pname = match pat {
            [TokenTree::Ident(id)] => id.to_string(),
            [TokenTree::Ident(m), TokenTree::Ident(id)] if m.to_string() == "mut" => id.to_string(),
            _ => return Err((span, "parameters must be plain names".into())),
        };
        let ty = p[colon + 1..].to_vec();
        params.push(Param { name: pname, ts: type_str(&ty), ty, attrs, span });
    }
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
        ret = Some(toks[start..i].to_vec());
    }
    Ok(Sig { name, name_span, vis, receiver, params, ret, doc })
}

/// The fn item with the binding attributes on its parameters removed (they are not real
/// attributes) and its own outer attributes filtered by `keep_attr`.
pub fn strip_fn(toks: &[TokenTree], keep_attr: &dyn Fn(&Attr) -> bool) -> TokenStream {
    let (attrs, start) = take_attrs(toks, 0);
    let mut out: Vec<TokenTree> = Vec::new();
    for a in &attrs {
        if keep_attr(a) {
            out.extend(a.tokens.iter().cloned());
        }
    }
    let mut seen_fn = false;
    let mut done = false;
    for t in &toks[start..] {
        if is_ident(t, "fn") {
            seen_fn = true;
        }
        match t {
            TokenTree::Group(g) if seen_fn && !done && g.delimiter() == Delimiter::Parenthesis => {
                done = true;
                let inner: Vec<TokenTree> = g.stream().into_iter().collect();
                let mut ps: Vec<TokenTree> = Vec::new();
                for (n, raw) in split_commas(&inner).into_iter().enumerate() {
                    if n > 0 {
                        ps.push(TokenTree::Punct(proc_macro::Punct::new(',', Spacing::Alone)));
                    }
                    let (pattrs, s) = take_attrs(&raw, 0);
                    for a in &pattrs {
                        if !PARAM_ATTRS.contains(&a.path.as_str()) {
                            ps.extend(a.tokens.iter().cloned());
                        }
                    }
                    ps.extend(raw[s..].iter().cloned());
                }
                let mut ng = Group::new(Delimiter::Parenthesis, ps.into_iter().collect());
                ng.set_span(g.span());
                out.push(TokenTree::Group(ng));
            }
            _ => out.push(t.clone()),
        }
    }
    out.into_iter().collect()
}
