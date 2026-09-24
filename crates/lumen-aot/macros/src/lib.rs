//! `include_js!` — see the `lumen-aot` crate for the user-facing documentation.

use std::path::PathBuf;

use proc_macro::{Delimiter, Literal, TokenStream, TokenTree};

#[path = "../../src/walk.rs"]
#[allow(dead_code)]
mod walk;

/// Precompile JavaScript at compile time into a `lumen::Precompiled` (see `lumen-aot`).
#[proc_macro]
pub fn include_js(input: TokenStream) -> TokenStream {
    match expand(input) {
        Ok(ts) => ts,
        Err(msg) => format!("compile_error!({:?})", format!("include_js!: {msg}"))
            .parse()
            .unwrap(),
    }
}

/// The value of a string literal token (plain or raw string).
fn string_lit(t: &TokenTree) -> Result<String, String> {
    let TokenTree::Literal(l) = t else {
        return Err(format!("expected a string literal, found `{t}`"));
    };
    let s = l.to_string();
    if let Some(body) = s.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        // Paths: only the escapes a path plausibly needs.
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                other => return Err(format!("unsupported escape \\{other:?} in {s}")),
            }
        }
        return Ok(out);
    }
    if let Some(rest) = s.strip_prefix('r') {
        let hashes = rest.len() - rest.trim_start_matches('#').len();
        let inner = &rest[hashes..];
        if let Some(body) = inner
            .strip_prefix('"')
            .and_then(|b| b.strip_suffix(&format!("\"{}", "#".repeat(hashes))))
        {
            return Ok(body.to_string());
        }
    }
    Err(format!("expected a string literal, found `{s}`"))
}

/// `"a"` or `["a", "b"]`.
fn string_list(t: &TokenTree) -> Result<Vec<String>, String> {
    match t {
        TokenTree::Group(g) if g.delimiter() == Delimiter::Bracket => g
            .stream()
            .into_iter()
            .filter(|t| !matches!(t, TokenTree::Punct(p) if p.as_char() == ','))
            .map(|t| string_lit(&t))
            .collect(),
        // A macro_rules! wrapper passes its fragment through an invisible group.
        TokenTree::Group(g) if g.delimiter() == Delimiter::None => {
            let inner: Vec<TokenTree> = g.stream().into_iter().collect();
            match inner.as_slice() {
                [one] => string_list(one),
                _ => Err(format!(
                    "expected a string or a list of strings, found `{t}`"
                )),
            }
        }
        t => Ok(vec![string_lit(t)?]),
    }
}

fn parse_spec(input: TokenStream) -> Result<walk::Spec, String> {
    let toks: Vec<TokenTree> = input.into_iter().collect();
    let mut spec = walk::Spec {
        walk: true,
        ..walk::Spec::default()
    };
    // Shorthand: a lone string literal is one script.
    if let [one] = toks.as_slice() {
        if let Ok(s) = string_lit(one) {
            spec.scripts.push(PathBuf::from(s));
            return Ok(spec);
        }
    }
    let mut i = 0;
    while i < toks.len() {
        let key = match &toks[i] {
            TokenTree::Ident(id) => id.to_string(),
            t => return Err(format!("expected `key = value`, found `{t}`")),
        };
        match toks.get(i + 1) {
            Some(TokenTree::Punct(p)) if p.as_char() == '=' => {}
            _ => return Err(format!("expected `=` after `{key}`")),
        }
        let value = toks
            .get(i + 2)
            .ok_or_else(|| format!("missing value for `{key}`"))?;
        match key.as_str() {
            "script" | "scripts" => {
                spec.scripts
                    .extend(string_list(value)?.into_iter().map(PathBuf::from));
            }
            "entry" | "module" => {
                if spec.entry.is_some() {
                    return Err("only one `entry`".into());
                }
                spec.entry = Some(PathBuf::from(string_lit(value)?));
            }
            "modules" => {
                spec.modules
                    .extend(string_list(value)?.into_iter().map(PathBuf::from));
            }
            "root" => spec.root = Some(PathBuf::from(string_lit(value)?)),
            "walk" => {
                spec.walk = match value.to_string().as_str() {
                    "true" => true,
                    "false" => false,
                    v => return Err(format!("`walk` takes true/false, found `{v}`")),
                }
            }
            "bytecode" => {
                spec.no_bytecode = match value.to_string().as_str() {
                    "true" => false,
                    "false" => true,
                    v => return Err(format!("`bytecode` takes true/false, found `{v}`")),
                }
            }
            "node_modules" => {
                spec.node_modules = match value.to_string().as_str() {
                    "true" => true,
                    "false" => false,
                    v => return Err(format!("`node_modules` takes true/false, found `{v}`")),
                }
            }
            "keep_source" => match value.to_string().as_str() {
                "true" => spec.keep_source = vec!["**".to_string()],
                "false" => spec.keep_source.clear(),
                _ => spec.keep_source.extend(string_list(value)?),
            },
            "exclude" => spec.exclude.extend(string_list(value)?),
            k => {
                return Err(format!(
                    "unknown key `{k}` (expected script, entry, modules, root, walk, bytecode, \
                     node_modules, keep_source, exclude)"
                ))
            }
        }
        i += 3;
        match toks.get(i) {
            None => {}
            Some(TokenTree::Punct(p)) if p.as_char() == ',' => i += 1,
            Some(t) => return Err(format!("expected `,`, found `{t}`")),
        }
    }
    if spec.scripts.is_empty() && spec.entry.is_none() && spec.modules.is_empty() {
        return Err("nothing to compile".into());
    }
    Ok(spec)
}

fn expand(input: TokenStream) -> Result<TokenStream, String> {
    let spec = parse_spec(input)?;
    let base = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .ok_or("CARGO_MANIFEST_DIR is not set")?;
    let bundle = walk::bundle(&base, &spec)?;

    let mut code = String::from("{\n");
    code.push_str(&format!(
        "const _: () = ::lumen_aot::__check_versions({}, {}, {});\n",
        lumen::precompiled::FORMAT_VERSION,
        lumen::precompiled::AST_VERSION,
        lumen::precompiled::LAYOUT_FINGERPRINT
    ));
    // Rebuild tracking only: an unreferenced const is never code-generated, so these bytes do
    // not reach the binary (see the crate docs).
    for input in &bundle.inputs {
        let path = input.to_string_lossy();
        code.push_str(&format!("const _: &[u8] = include_bytes!({path:?});\n"));
    }
    code.push_str("::lumen_aot::Precompiled::from_static(BLOB)\n}");
    // Splice the blob in as a real literal token (no string round-trip through the lexer).
    let mut out: Vec<TokenTree> = Vec::new();
    for t in code.parse::<TokenStream>().map_err(|e| e.to_string())? {
        out.push(t);
    }
    let TokenTree::Group(block) = &out[0] else {
        unreachable!()
    };
    let inner: TokenStream = block
        .stream()
        .into_iter()
        .map(|t| match t {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis => {
                let s: TokenStream = g
                    .stream()
                    .into_iter()
                    .map(|t| match &t {
                        TokenTree::Ident(id) if id.to_string() == "BLOB" => {
                            TokenTree::Literal(Literal::byte_string(&bundle.blob))
                        }
                        _ => t,
                    })
                    .collect();
                let mut ng = proc_macro::Group::new(Delimiter::Parenthesis, s);
                ng.set_span(g.span());
                TokenTree::Group(ng)
            }
            t => t,
        })
        .collect();
    Ok(TokenStream::from(TokenTree::Group(proc_macro::Group::new(
        Delimiter::Brace,
        inner,
    ))))
}
