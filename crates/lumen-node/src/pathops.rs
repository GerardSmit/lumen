//! POSIX `node:path` operations the module loader needs, natively, so a program that never
//! requires `path` does not load its JS. Ports of the POSIX half of Node's `lib/path.js`.

use lumen_host::{Ctx, Value};

fn arg(ctx: &mut Ctx, args: &[Value], i: usize) -> Result<String, Value> {
    Ok(ctx
        .coerce_string(args.get(i).unwrap_or(&Value::Undefined))?
        .to_string())
}

/// Node's `normalizeString` for POSIX separators.
fn normalize_string(path: &str, allow_above_root: bool) -> String {
    let b = path.as_bytes();
    let mut res = String::new();
    let mut last_segment_length = 0usize;
    let mut last_slash: isize = -1;
    let mut dots: i32 = 0;
    let mut code = 0u8;
    for i in 0..=b.len() {
        if i < b.len() {
            code = b[i];
        } else if code == b'/' {
            break;
        } else {
            code = b'/';
        }
        if code == b'/' {
            if last_slash == i as isize - 1 || dots == 1 {
            } else if dots == 2 {
                let rb = res.as_bytes();
                if rb.len() < 2
                    || last_segment_length != 2
                    || rb[rb.len() - 1] != b'.'
                    || rb[rb.len() - 2] != b'.'
                {
                    if res.len() > 2 {
                        match res.rfind('/') {
                            None => {
                                res.clear();
                                last_segment_length = 0;
                            }
                            Some(at) => {
                                res.truncate(at);
                                last_segment_length =
                                    res.len() - 1 - res.rfind('/').map_or(-1, |x| x as isize) as usize;
                            }
                        }
                        last_slash = i as isize;
                        dots = 0;
                        continue;
                    } else if !res.is_empty() {
                        res.clear();
                        last_segment_length = 0;
                        last_slash = i as isize;
                        dots = 0;
                        continue;
                    }
                }
                if allow_above_root {
                    res.push_str(if res.is_empty() { ".." } else { "/.." });
                    last_segment_length = 2;
                }
            } else {
                let seg = &path[(last_slash + 1) as usize..i];
                if !res.is_empty() {
                    res.push('/');
                }
                res.push_str(seg);
                last_segment_length = (i as isize - last_slash - 1) as usize;
            }
            last_slash = i as isize;
            dots = 0;
        } else if code == b'.' && dots != -1 {
            dots += 1;
        } else {
            dots = -1;
        }
    }
    res
}

fn normalize(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let absolute = path.starts_with('/');
    let trailing = path.ends_with('/');
    let mut out = normalize_string(path, !absolute);
    if out.is_empty() {
        return if absolute { "/" } else if trailing { "./" } else { "." }.into();
    }
    if trailing {
        out.push('/');
    }
    if absolute {
        out.insert(0, '/');
    }
    out
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
    let only_cwd = match args {
        [] => true,
        [_] => matches!(arg(ctx, args, 0)?.as_str(), "" | "."),
        _ => false,
    };
    if only_cwd {
        let cwd = cwd(ctx)?;
        if cwd.starts_with('/') {
            return Ok(Value::from_string(cwd));
        }
    }
    let mut resolved = String::new();
    let mut absolute = false;
    let mut i = args.len() as isize - 1;
    while i >= -1 && !absolute {
        let p = if i >= 0 { arg(ctx, args, i as usize)? } else { cwd(ctx)? };
        i -= 1;
        if p.is_empty() {
            continue;
        }
        resolved = format!("{p}/{resolved}");
        absolute = p.starts_with('/');
    }
    let out = normalize_string(&resolved, !absolute);
    Ok(Value::from_string(if absolute {
        format!("/{out}")
    } else if out.is_empty() {
        ".".into()
    } else {
        out
    }))
}

/// `(...paths) -> string` — `path.posix.join`.
pub(crate) fn op_join(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let mut joined: Option<String> = None;
    for i in 0..args.len() {
        let p = arg(ctx, args, i)?;
        if p.is_empty() {
            continue;
        }
        joined = Some(match joined {
            None => p,
            Some(j) => format!("{j}/{p}"),
        });
    }
    Ok(Value::from_string(joined.map_or_else(|| ".".into(), |j| normalize(&j))))
}

fn dirname(path: &str) -> &str {
    let b = path.as_bytes();
    if b.is_empty() {
        return ".";
    }
    let has_root = b[0] == b'/';
    let mut end = None;
    let mut matched_slash = true;
    for i in (1..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                end = Some(i);
                break;
            }
        } else {
            matched_slash = false;
        }
    }
    match end {
        None => {
            if has_root {
                "/"
            } else {
                "."
            }
        }
        Some(1) if has_root => "//",
        Some(end) => &path[..end],
    }
}

/// `(path) -> string` — `path.posix.dirname`.
pub(crate) fn op_dirname(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    Ok(Value::from_string(dirname(&p).into()))
}

fn basename<'a>(path: &'a str, suffix: Option<&str>) -> &'a str {
    let b = path.as_bytes();
    let mut start = 0usize;
    let mut end: isize = -1;
    let mut matched_slash = true;
    if let Some(suffix) = suffix.filter(|s| !s.is_empty() && s.len() <= path.len()) {
        if suffix == path {
            return "";
        }
        let sb = suffix.as_bytes();
        let mut ext_idx = sb.len() as isize - 1;
        let mut first_non_slash_end: isize = -1;
        for i in (0..b.len()).rev() {
            let code = b[i];
            if code == b'/' {
                if !matched_slash {
                    start = i + 1;
                    break;
                }
            } else {
                if first_non_slash_end == -1 {
                    matched_slash = false;
                    first_non_slash_end = i as isize + 1;
                }
                if ext_idx >= 0 {
                    if code == sb[ext_idx as usize] {
                        ext_idx -= 1;
                        if ext_idx == -1 {
                            end = i as isize;
                        }
                    } else {
                        ext_idx = -1;
                        end = first_non_slash_end;
                    }
                }
            }
        }
        if start as isize == end {
            end = first_non_slash_end;
        } else if end == -1 {
            end = b.len() as isize;
        }
        return &path[start..end as usize];
    }
    for i in (0..b.len()).rev() {
        if b[i] == b'/' {
            if !matched_slash {
                start = i + 1;
                break;
            }
        } else if end == -1 {
            matched_slash = false;
            end = i as isize + 1;
        }
    }
    if end == -1 {
        ""
    } else {
        &path[start..end as usize]
    }
}

/// `(path, suffix?) -> string` — `path.posix.basename`.
pub(crate) fn op_basename(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    let suffix = match args.get(1) {
        None | Some(Value::Undefined) => None,
        Some(v) => Some(ctx.coerce_string(v)?.to_string()),
    };
    Ok(Value::from_string(basename(&p, suffix.as_deref()).into()))
}

fn extname(path: &str) -> &str {
    let b = path.as_bytes();
    let mut start_dot: isize = -1;
    let mut start_part: isize = 0;
    let mut end: isize = -1;
    let mut matched_slash = true;
    let mut pre_dot_state = 0;
    for i in (0..b.len()).rev() {
        let code = b[i];
        if code == b'/' {
            if !matched_slash {
                start_part = i as isize + 1;
                break;
            }
            continue;
        }
        if end == -1 {
            matched_slash = false;
            end = i as isize + 1;
        }
        if code == b'.' {
            if start_dot == -1 {
                start_dot = i as isize;
            } else if pre_dot_state != 1 {
                pre_dot_state = 1;
            }
        } else if start_dot != -1 {
            pre_dot_state = -1;
        }
    }
    if start_dot == -1
        || end == -1
        || pre_dot_state == 0
        || (pre_dot_state == 1 && start_dot == end - 1 && start_dot == start_part + 1)
    {
        return "";
    }
    &path[start_dot as usize..end as usize]
}

/// `(path) -> string` — `path.posix.extname`.
pub(crate) fn op_extname(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let p = arg(ctx, args, 0)?;
    Ok(Value::from_string(extname(&p).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_node_posix_edge_cases() {
        assert_eq!(normalize("/a/b/../c/"), "/a/c/");
        assert_eq!(normalize("../x"), "../x");
        assert_eq!(normalize("./"), "./");
        assert_eq!(normalize("/.."), "/");
        assert_eq!(normalize("a/../.."), "..");
        assert_eq!(dirname("/./a//b"), "/./a/");
        assert_eq!(dirname("//a//b"), "//a/");
        assert_eq!(dirname("//"), "/");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(basename("/a/b.c//", Some(".c")), "b");
        assert_eq!(basename("aaa", Some("a")), "aa");
        assert_eq!(basename("/x/", None), "x");
        assert_eq!(extname("/a//b/./c/.."), "");
        assert_eq!(extname(".d"), "");
        assert_eq!(extname("e."), ".");
        assert_eq!(extname("f.g.h"), ".h");
    }
}
