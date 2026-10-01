//! Node's `.env` file grammar (`--env-file`): `KEY=value` lines, `#` comments, an optional
//! `export ` prefix, single/double/backtick quoted values and `\n` expansion in double quotes.

pub fn parse(content: &str) -> Vec<(String, String)> {
    let content = content.replace("\r\n", "\n");
    let mut rest = content.as_str();
    let mut out = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t', '\n']);
        if rest.is_empty() {
            break;
        }
        if rest.starts_with('#') {
            rest = rest.find('\n').map_or("", |i| &rest[i..]);
            continue;
        }
        let eq = rest.find('=');
        let nl = rest.find('\n');
        let eq = match (eq, nl) {
            (Some(e), Some(n)) if e > n => {
                rest = &rest[n..];
                continue;
            }
            (Some(e), _) => e,
            (None, _) => break,
        };
        let mut key = rest[..eq].trim();
        if let Some(k) = key.strip_prefix("export ") {
            key = k.trim_start();
        }
        rest = &rest[eq + 1..];
        let key = key.to_string();
        rest = rest.trim_start_matches([' ', '\t']);
        if key.is_empty() {
            rest = rest.find('\n').map_or("", |i| &rest[i..]);
            continue;
        }
        let first = rest.chars().next();
        if let Some(q @ ('"' | '\'' | '`')) = first {
            if let Some(close) = rest[1..].find(q) {
                let mut value = rest[1..1 + close].to_string();
                if q == '"' {
                    value = value.replace("\\n", "\n");
                }
                rest = &rest[close + 2..];
                rest = rest.find('\n').map_or("", |i| &rest[i..]);
                out.push((key, value));
                continue;
            }
        }
        let end = rest.find('\n').unwrap_or(rest.len());
        let mut value = &rest[..end];
        if let Some(h) = value.find('#') {
            value = &value[..h];
        }
        out.push((key, value.trim().to_string()));
        rest = &rest[end..];
    }
    out
}
