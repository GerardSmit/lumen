//! POSIX `node:path` operations the module loader needs, natively, so a program that never
//! requires `path` does not load its JS. Results match `js/path.js`'s posix implementation.

use lumen_host::{Ctx, Value};

fn arg(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

/// `s.split(/\/+/)`.
fn split_runs(s: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut in_run = false;
    for (at, c) in s.char_indices() {
        if c == '/' {
            if !in_run {
                parts.push(&s[start..at]);
                in_run = true;
            }
        } else if in_run {
            start = at;
            in_run = false;
        }
    }
    parts.push(if in_run { "" } else { &s[start..] });
    parts
}

fn normalize_parts<'a>(parts: Vec<&'a str>, allow_above_root: bool) -> Vec<&'a str> {
    let mut res: Vec<&str> = Vec::new();
    for p in parts {
        match p {
            "" | "." => {}
            ".." => {
                if res.last().is_some_and(|last| *last != "..") {
                    res.pop();
                } else if allow_above_root {
                    res.push("..");
                }
            }
            _ => res.push(p),
        }
    }
    res
}

fn normalize(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let absolute = p.starts_with('/');
    let trailing = p.ends_with(['/', '\\']);
    let mut parts = normalize_parts(split_runs(p), !absolute).join("/");
    if parts.is_empty() && !absolute {
        parts.push('.');
    }
    if !parts.is_empty() && trailing {
        parts.push('/');
    }
    if absolute {
        parts.insert(0, '/');
    }
    parts
}

fn cwd(ctx: &mut Ctx) -> Result<String, Value> {
    if let Some(realm) = ctx.op_state().get::<lumen_host::RealmProcess>() {
        return Ok(realm.cwd.to_string_lossy().into_owned());
    }
    match std::env::current_dir() {
        Ok(p) => Ok(p.to_string_lossy().into_owned()),
        Err(e) => Err(ctx.make_error("Error", format!("cwd unavailable: {e}"))),
    }
}

/// `(...paths) -> string` — `path.posix.resolve`.
pub(crate) fn op_resolve(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut resolved = String::new();
    let mut absolute = false;
    for i in (0..args.len()).rev() {
        let p = arg(ctx, args, i)?;
        if p.is_empty() {
            continue;
        }
        resolved = format!("{p}/{resolved}");
        if p.starts_with('/') {
            absolute = true;
            break;
        }
    }
    if !absolute {
        resolved = format!("{}/{resolved}", cwd(ctx)?);
    }
    let parts = normalize_parts(split_runs(&resolved), false).join("/");
    Ok(Value::from_string(format!("/{parts}")))
}

/// `(...paths) -> string` — `path.posix.join`.
pub(crate) fn op_join(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut joined = String::new();
    let mut any = false;
    for (i, v) in args.iter().enumerate() {
        if matches!(v, Value::Undefined | Value::Null) {
            continue;
        }
        let p = arg(ctx, args, i)?;
        if p.is_empty() {
            continue;
        }
        if any {
            joined.push('/');
        }
        joined.push_str(&p);
        any = true;
    }
    Ok(Value::from_string(if any { normalize(&joined) } else { ".".into() }))
}

/// `(path) -> string` — `path.posix.dirname`.
pub(crate) fn op_dirname(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    let mut parts = split_runs(&p);
    while parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    let root = if p.starts_with('/') { "/" } else { "." };
    if parts.len() <= 1 {
        return Ok(Value::from_string(root.into()));
    }
    parts.pop();
    let dir = parts.join("/");
    Ok(Value::from_string(if dir.is_empty() { root.into() } else { dir }))
}

fn basename(p: &str) -> &str {
    split_runs(p).into_iter().rfind(|x| !x.is_empty()).unwrap_or("")
}

/// `(path, ext?) -> string` — `path.posix.basename`.
pub(crate) fn op_basename(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    let mut base = basename(&p);
    if let Some(ext) = args.get(1).filter(|v| !matches!(v, Value::Undefined | Value::Null)) {
        let ext = ctx.coerce_string(ext)?.to_string();
        if !ext.is_empty() && base != ext {
            base = base.strip_suffix(ext.as_str()).unwrap_or(base);
        }
    }
    Ok(Value::from_string(base.into()))
}

/// `(path) -> string` — `path.posix.extname`.
pub(crate) fn op_extname(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    let base = basename(&p);
    Ok(Value::from_string(match base.rfind('.') {
        Some(i) if i > 0 => base[i..].into(),
        _ => String::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_matches_regex_split() {
        assert_eq!(split_runs(""), [""]);
        assert_eq!(split_runs("/"), ["", ""]);
        assert_eq!(split_runs("a//b/"), ["a", "b", ""]);
        assert_eq!(split_runs("//a"), ["", "a"]);
    }

    #[test]
    fn normalize_keeps_trailing_separator() {
        assert_eq!(normalize("/a/b/../c/"), "/a/c/");
        assert_eq!(normalize("../x"), "../x");
        assert_eq!(normalize("./"), "./");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a\\"), "a\\/");
    }
}
