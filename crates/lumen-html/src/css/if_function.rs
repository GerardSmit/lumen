//! `if()`: `if(<condition>: <value>; ...; else: <value>)` replaced by the value of the first
//! branch whose condition holds. Conditions combine `style()`, `media()` and `supports()` tests.
//! Evaluation happens where `--name()` calls do, against the element's custom properties.
use super::container_query::{self, Kind, Token};
use super::registered_properties::RegisteredCustomProperty;
use super::*;

const MAX_IFS: usize = 64;

pub(super) struct IfScope<'a> {
    pub names: &'a dyn CustomNames,
    pub base: Option<&'a dyn CustomNames>,
    pub registrations: &'a [RegisteredCustomProperty],
    pub context: &'a CustomContext,
    pub environment: MediaEnvironment,
    pub current: Option<&'a str>,
}

fn name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' || !byte.is_ascii()
}

/// Outermost `if(` calls of `raw` outside strings and comments, as `(start, open, close)`.
fn find_ifs(raw: &str) -> Vec<(usize, usize, usize)> {
    let mut found = Vec::new();
    let Ok(mut cursor) = syntax::Cursor::new(raw, 0) else { return found };
    while let Some(token) = cursor.next() {
        if token.kind == syntax::TokenKind::Other
            && raw[token.start..token.end].eq_ignore_ascii_case("if")
            && raw.as_bytes().get(token.end) == Some(&b'(')
        {
            let open = token.end;
            let Some(close) = syntax::matching_close(raw, open) else { break };
            found.push((token.start, open, close));
            cursor.position = close + 1;
        }
    }
    found
}

pub(super) fn contains_if(raw: &str) -> bool {
    raw.len() >= 3 && raw.as_bytes().windows(3).any(|window| window[0] | 0x20 == b'i' && window[1] | 0x20 == b'f' && window[2] == b'(')
        && !find_ifs(raw).is_empty()
}

struct Branch<'a> {
    condition: &'a str,
    value: &'a str,
}

/// A `<declaration-value>` takes no top-level `!`.
fn declaration_value(text: &str) -> bool {
    syntax::top_level_find(text, b'!') == Some(None)
}

fn branches(inner: &str) -> Option<Vec<Branch<'_>>> {
    let mut parts = syntax::split_top_level(inner, b';', usize::MAX, usize::MAX)?;
    if parts.last().is_some_and(|last| last.trim().is_empty()) {
        parts.pop();
    }
    if parts.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(parts.len());
    for part in parts {
        let colon = syntax::top_level_find(part, b':').flatten()?;
        let condition = part[..colon].trim();
        let value = part[colon + 1..].trim_start();
        if condition.is_empty() || !declaration_value(value) {
            return None;
        }
        out.push(Branch { condition, value });
    }
    Some(out)
}

fn var_references(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    let mut names = Vec::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find("var(") {
        let start = from + found + 4;
        let rest = text[start..].trim_start();
        let end = rest.find(|ch: char| ch.is_whitespace() || ch == ',' || ch == ')').unwrap_or(rest.len());
        names.push(String::from(&rest[..end]));
        from = start;
    }
    names
}

fn dashed_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut names = Vec::new();
    let mut position = 0;
    while position + 2 < bytes.len() {
        if bytes[position] == b'-' && bytes[position + 1] == b'-' && (position == 0 || !name_byte(bytes[position - 1])) {
            let mut end = position + 2;
            while end < bytes.len() && name_byte(bytes[end]) {
                end += 1;
            }
            if end > position + 2 {
                names.push(String::from(&text[position..end]));
            }
            position = end;
        } else {
            position += 1;
        }
    }
    names
}

fn reaches(name: &str, target: &str, names: &dyn CustomNames, visited: &mut Vec<String>) -> bool {
    if name == target {
        return true;
    }
    if visited.iter().any(|seen| seen == name) || visited.len() > 256 {
        return false;
    }
    visited.push(String::from(name));
    let Some(value) = names.value(name) else { return false };
    var_references(value.as_str()).iter().any(|next| reaches(next, target, names, visited))
}

impl IfScope<'_> {
    fn registration(&self, name: &str) -> Option<&RegisteredCustomProperty> {
        self.registrations.iter().find(|registration| registration.name == name)
    }

    fn normalize(&self, registration: Option<&RegisteredCustomProperty>, text: &str) -> Option<String> {
        let text = text.trim();
        match registration {
            Some(registration) => registered_properties::compute_value_with_source(&registration.syntax, text,
                self.context.0, self.context.1, &self.context.2, registration.source_url.as_deref()),
            None => Some(container_query::collapse_whitespace(text)),
        }
    }

    fn initial(&self, registration: Option<&RegisteredCustomProperty>) -> Option<String> {
        let registration = registration?;
        registration.initial_value.as_deref().and_then(|value| self.normalize(Some(registration), value))
    }

    fn value_of(&self, names: &dyn CustomNames, name: &str, registration: Option<&RegisteredCustomProperty>) -> Option<String> {
        let raw = names.value(name)?;
        let text = if raw.expands() {
            var_template::expand_variables_in(raw.as_str(), names, MAX_VARIABLE_BYTES)?
        } else {
            String::from(raw.as_str())
        };
        match registration {
            Some(_) => self.normalize(registration, &text).or_else(|| self.initial(registration)),
            None => self.normalize(None, &text),
        }
    }

    pub(super) fn declared(&self, name: &str, wanted: Option<&str>) -> Option<bool> {
        let registration = self.registration(name);
        let current = self.value_of(self.names, name, registration)
            .or_else(|| if self.names.value(name).is_none() { self.initial(registration) } else { None });
        let Some(wanted) = wanted else { return Some(current.is_some()) };
        let keyword = wanted.trim().to_ascii_lowercase();
        let inherited = registration.is_none_or(|registration| registration.inherits);
        let as_inherit = || {
            let parent = self.base.and_then(|base| self.value_of(base, name, registration)).or_else(|| self.initial(registration));
            current == parent
        };
        let as_initial = || current == self.initial(registration);
        match keyword.as_str() {
            "initial" => Some(as_initial()),
            "inherit" => Some(as_inherit()),
            "unset" => Some(if inherited { as_inherit() } else { as_initial() }),
            "revert" | "revert-layer" | "revert-rule" => Some(false),
            _ => {
                let Some(expected) = self.normalize(registration, wanted) else { return Some(false) };
                Some(current.as_deref() == Some(expected.as_str()))
            }
        }
    }

    fn cyclic(&self, condition: &str) -> bool {
        let Some(target) = self.current else { return false };
        dashed_names(condition).iter().any(|name| reaches(name, target, self.names, &mut Vec::new()))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Truth {
    True,
    False,
    Unknown,
}

fn media_truth(content: &str, scope: &IfScope<'_>) -> Truth {
    let content = content.trim();
    if content.is_empty() {
        return Truth::Unknown;
    }
    let query = if content.starts_with('(') || content.to_ascii_lowercase().starts_with("not ") {
        String::from(content)
    } else {
        alloc::format!("({content})")
    };
    if media_queries::query_matches(&query, scope.environment) { Truth::True } else { Truth::False }
}

fn supports_truth(content: &str) -> Truth {
    let content = content.trim();
    let lower = content.to_ascii_lowercase();
    let condition = if content.starts_with('(') || lower.starts_with("not ") || lower.starts_with("not(") || lower.starts_with("selector(") {
        String::from(content)
    } else {
        alloc::format!("({content})")
    };
    match parse_supports_condition(&condition, 0, &NamespaceMap::default()) {
        Some(true) => Truth::True,
        Some(false) => Truth::False,
        None => Truth::Unknown,
    }
}

fn style_truth(content: &str, scope: &IfScope<'_>) -> Truth {
    if content.trim().is_empty() {
        return Truth::Unknown;
    }
    let length = || scope.context.0;
    let declared = |name: &str, wanted: Option<&str>| scope.declared(name, wanted);
    match container_eval::style_query_truth(content, scope.names, &length, &declared) {
        Some(true) => Truth::True,
        Some(false) => Truth::False,
        None => Truth::Unknown,
    }
}

fn term(token: &Token<'_>, scope: &IfScope<'_>) -> Truth {
    match token.kind {
        Kind::Paren => {
            let inner = container_query::inner(token, 0);
            match container_query::tokenize(&inner) {
                Some(tokens) if !tokens.is_empty() => condition(&tokens, scope),
                _ => Truth::Unknown,
            }
        }
        Kind::Function => {
            let Some(open) = token.text.find('(') else { return Truth::Unknown };
            let content = container_query::inner(token, open);
            let name = token.text[..open].to_ascii_lowercase();
            match name.as_str() {
                "style" => style_truth(&content, scope),
                "media" => media_truth(&content, scope),
                "supports" => supports_truth(&content),
                _ => Truth::Unknown,
            }
        }
        Kind::Ident | Kind::Other => Truth::Unknown,
    }
}

fn condition(tokens: &[Token<'_>], scope: &IfScope<'_>) -> Truth {
    let Some(first) = tokens.first() else { return Truth::Unknown };
    if container_query::keyword(first, "not") {
        if tokens.len() != 2 {
            return Truth::Unknown;
        }
        return match term(&tokens[1], scope) {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        };
    }
    let head = term(first, scope);
    if tokens.len() == 1 {
        return head;
    }
    let all = container_query::keyword(&tokens[1], "and");
    if tokens.len() % 2 == 0 || !(all || container_query::keyword(&tokens[1], "or")) {
        return Truth::Unknown;
    }
    let mut result = head;
    for pair in tokens[1..].chunks_exact(2) {
        if !container_query::keyword(&pair[0], if all { "and" } else { "or" }) {
            return Truth::Unknown;
        }
        let next = term(&pair[1], scope);
        result = match (all, result, next) {
            (true, Truth::False, _) | (true, _, Truth::False) => Truth::False,
            (true, Truth::True, Truth::True) => Truth::True,
            (false, Truth::True, _) | (false, _, Truth::True) => Truth::True,
            (false, Truth::False, Truth::False) => Truth::False,
            _ => Truth::Unknown,
        };
    }
    result
}

/// `Err` is an invalid `if()`; `Ok(None)` means no branch holds.
fn evaluate<'a>(inner: &'a str, scope: &IfScope<'_>) -> Result<Option<&'a str>, ()> {
    let branches = branches(inner).ok_or(())?;
    if branches.iter().any(|branch| branch.condition.starts_with('!') || branch.condition.starts_with(',')) {
        return Err(());
    }
    for branch in &branches {
        let text = branch.condition;
        let expanded;
        let text = if text.to_ascii_lowercase().starts_with("var(") {
            if scope.cyclic(text) {
                return Err(());
            }
            expanded = var_template::expand_variables_in(text, scope.names, MAX_VARIABLE_BYTES);
            match expanded.as_deref() {
                Some(text) => text,
                None => continue,
            }
        } else {
            text
        };
        if text.eq_ignore_ascii_case("else") {
            return Ok(Some(branch.value));
        }
        if scope.cyclic(text) {
            return Err(());
        }
        let Some(tokens) = container_query::tokenize(text) else { continue };
        if tokens.first().is_some_and(|first| first.kind == Kind::Other) {
            return Err(());
        }
        if condition(&tokens, scope) == Truth::True {
            return Ok(Some(branch.value));
        }
    }
    Ok(None)
}

/// `raw` with every `if()` replaced by its chosen value; `None` when one is invalid or no branch holds.
pub(super) fn replace_ifs(raw: &str, scope: &IfScope<'_>) -> Option<String> {
    let found = find_ifs(raw);
    if found.len() > MAX_IFS {
        return None;
    }
    let mut out = String::with_capacity(raw.len());
    let (mut last, mut previous_end) = (0, None);
    for (start, open, close) in found {
        out.push_str(&raw[last..start]);
        let chosen = evaluate(&raw[open + 1..close], scope).ok()??;
        if previous_end == Some(start) {
            out.push_str("/**/");
        }
        out.push_str(chosen);
        last = close + 1;
        previous_end = Some(last);
    }
    out.push_str(&raw[last..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_outermost_calls_only() {
        let found = find_ifs("a if(x: if(y: 1)) 'if(z)' xif(q) if(w: 2)");
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn splits_branches() {
        let parsed = branches("style(--x: 3): a; else: b ;").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].condition, "style(--x: 3)");
        assert_eq!(parsed[1].value, "b ");
        assert!(branches("").is_none());
        assert!(branches("style(--x)").is_none());
        assert!(branches("style(--x): a!").is_none());
    }
}
