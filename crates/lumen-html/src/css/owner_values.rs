//! Element-dependent substitution functions: `attr()`, `random()` and `random-item()`.
//!
//! These functions need the element a declaration applies to. A declaration using them is
//! deferred like one using `var()`; at cascade time the calls are replaced by text and the
//! result is parsed as an ordinary value.

use super::registered_properties;
use super::syntax::{self, Cursor, TokenKind};
use super::typed_numeric::{self, NumericUnit, NumericValue};
use super::*;
use core::cell::{Cell, RefCell};

const MAX_DEPTH: usize = 8;
const MAX_ARGUMENTS: usize = 4096;
const MAX_STEPS: usize = 4096;
const MAX_TOTAL_BYTES: usize = 4 * MAX_VARIABLE_BYTES;

pub(super) struct Owner<'a> {
    pub element: Option<(&'a Document, NodeId)>,
    pub property: &'a str,
}

struct Scope<'a> {
    owner: &'a Owner<'a>,
    index: Cell<usize>,
    attributes: RefCell<Vec<String>>,
    steps: Cell<usize>,
    bytes: Cell<usize>,
}

impl<'a> Scope<'a> {
    fn new(owner: &'a Owner<'a>) -> Self {
        Self {
            owner,
            index: Cell::new(0),
            attributes: RefCell::new(Vec::new()),
            steps: Cell::new(0),
            bytes: Cell::new(0),
        }
    }

    fn charge(&self, bytes: usize) -> Option<()> {
        let steps = self.steps.get() + 1;
        let total = self.bytes.get().saturating_add(bytes);
        if steps > MAX_STEPS || total > MAX_TOTAL_BYTES {
            return None;
        }
        self.steps.set(steps);
        self.bytes.set(total);
        Some(())
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Attr,
    Random,
    RandomItem,
    Ident,
}

struct Call {
    start: usize,
    open: usize,
    after: usize,
    kind: Kind,
}

fn kind_of(name: &str) -> Option<Kind> {
    if name.eq_ignore_ascii_case("attr") {
        Some(Kind::Attr)
    } else if name.eq_ignore_ascii_case("random") {
        Some(Kind::Random)
    } else if name.eq_ignore_ascii_case("random-item") {
        Some(Kind::RandomItem)
    } else if name.eq_ignore_ascii_case("ident") {
        Some(Kind::Ident)
    } else {
        None
    }
}

fn mentions(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.contains(&b'\\')
        || bytes.windows(5).any(|window| window.eq_ignore_ascii_case(b"attr("))
        || bytes.windows(7).any(|window| window.eq_ignore_ascii_case(b"random("))
        || bytes.windows(12).any(|window| window.eq_ignore_ascii_case(b"random-item("))
        || bytes.windows(6).any(|window| window.eq_ignore_ascii_case(b"ident("))
}

/// Whether a custom property value contains an `attr()` call to substitute.
pub(super) fn uses_attr(raw: &str) -> bool {
    raw.as_bytes().windows(5).any(|window| window.eq_ignore_ascii_case(b"attr(")) && may_use(raw)
}

/// Whether a custom property value contains a `random()` or `random-item()` call to substitute.
pub(super) fn uses_random(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (bytes.windows(7).any(|window| window.eq_ignore_ascii_case(b"random("))
        || bytes.windows(12).any(|window| window.eq_ignore_ascii_case(b"random-item(")))
        && may_use(raw)
}

/// Outermost calls of `raw`; `None` when a call is not closed.
fn find_calls(raw: &str) -> Option<Vec<Call>> {
    let mut calls = Vec::new();
    let mut cursor = Cursor::new(raw, 0).ok()?;
    while let Some(token) = cursor.next() {
        if token.kind != TokenKind::Other || raw.as_bytes().get(token.end) != Some(&b'(') {
            continue;
        }
        let mut at = token.start;
        let Some(name) = consume_selector_identifier(raw, &mut at).filter(|_| at == token.end) else { continue };
        let Some(kind) = kind_of(&name) else { continue };
        let block = syntax::block(raw, token.end).ok()?;
        if !block.closed {
            return None;
        }
        calls.push(Call { start: token.start, open: token.end, after: block.after, kind });
        cursor.position = block.after;
    }
    Some(calls)
}

/// A value naming one of these functions whose call never closes and that
/// holds a closing token matching no open block is not a valid
/// `<declaration-value>`.
pub(super) fn has_unmatched_closer(raw: &str) -> bool {
    if !mentions(raw) || find_calls(raw).is_some() {
        return false;
    }
    let Ok(mut cursor) = Cursor::new(raw, 0) else { return false };
    let mut stack = Vec::new();
    while let Some(token) = cursor.next() {
        match token.kind {
            TokenKind::Open(open) => stack.push(open),
            TokenKind::Close(close) => {
                let expected = match close {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if stack.last() == Some(&expected) {
                    stack.pop();
                } else {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Whether the value names an element-dependent function.
pub(super) fn may_use(raw: &str) -> bool {
    mentions(raw) && find_calls(raw).is_some_and(|calls| !calls.is_empty())
}

fn inner<'a>(raw: &'a str, call: &Call) -> &'a str {
    &raw[call.open + 1..call.after - 1]
}

#[derive(Clone, Copy, PartialEq)]
enum Ua<'a> {
    Property,
    PropertyIndex,
    Named(&'a str),
}

struct Caching<'a> {
    auto: bool,
    fixed: Option<&'a str>,
    ident: Option<&'a str>,
    element: bool,
    ua: Option<Ua<'a>>,
}

fn parse_caching(part: &str) -> Option<Caching<'_>> {
    let tokens = components(part)?;
    if tokens.is_empty() {
        return None;
    }
    let mut caching = Caching { auto: false, fixed: None, ident: None, element: false, ua: None };
    let mut at = 0;
    while at < tokens.len() {
        let token = tokens[at];
        let lower = ascii_lower(token);
        match &*lower {
            "auto" if tokens.len() == 1 => caching.auto = true,
            "fixed" if tokens.len() == 2 => {
                let value = tokens[1];
                if let Some(number) = typed_numeric::parse_numeric_value(value) {
                    if number.unit != NumericUnit::Number || number.value < 0.0 {
                        return None;
                    }
                }
                caching.fixed = Some(value);
                at += 1;
            }
            "element-scoped" if !caching.element => caching.element = true,
            "property-scoped" if caching.ua.is_none() => caching.ua = Some(Ua::Property),
            "property-index-scoped" if caching.ua.is_none() => caching.ua = Some(Ua::PropertyIndex),
            _ if token.starts_with("--") && token.len() > 2 && caching.ident.is_none() => caching.ident = Some(token),
            _ if lower.starts_with("ua-") && token.len() > 3 && caching.ua.is_none() => caching.ua = Some(Ua::Named(token)),
            _ => return None,
        }
        at += 1;
    }
    Some(caching)
}

struct RandomArguments<'a> {
    caching: Option<&'a str>,
    min: &'a str,
    max: &'a str,
    step: Option<&'a str>,
}

fn random_arguments(inner: &str) -> Option<RandomArguments<'_>> {
    let parts = syntax::split_top_level(inner, b',', 5, MAX_CSS_BYTES)?;
    if parts.iter().any(|part| part.trim().is_empty()) {
        return None;
    }
    match parts.len() {
        2 => Some(RandomArguments { caching: None, min: parts[0], max: parts[1], step: None }),
        3 if parse_caching(parts[0]).is_some() => {
            Some(RandomArguments { caching: Some(parts[0]), min: parts[1], max: parts[2], step: None })
        }
        3 => Some(RandomArguments { caching: None, min: parts[0], max: parts[1], step: Some(parts[2]) }),
        4 => {
            parse_caching(parts[0])?;
            Some(RandomArguments { caching: Some(parts[0]), min: parts[1], max: parts[2], step: Some(parts[3]) })
        }
        _ => None,
    }
}

struct ItemArguments<'a> {
    caching: &'a str,
    items: Vec<&'a str>,
}

fn random_item_arguments(inner: &str) -> Option<ItemArguments<'_>> {
    let parts = syntax::split_top_level(inner, b',', MAX_ARGUMENTS, MAX_CSS_BYTES)?;
    if parts.len() < 2 || parts[0].trim().is_empty() {
        return None;
    }
    let mut items = Vec::with_capacity(parts.len() - 1);
    for part in &parts[1..] {
        let part = part.trim();
        if part.starts_with('{') {
            let block = syntax::block(part, 0).ok()?;
            if !block.closed || block.after != part.len() {
                return None;
            }
            items.push(part[1..part.len() - 1].trim());
            continue;
        }
        if part.contains('{') || syntax::top_level_find(part, b'!')? .is_some() || syntax::top_level_find(part, b';')?.is_some() {
            return None;
        }
        items.push(part);
    }
    Some(ItemArguments { caching: parts[0], items })
}

fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

fn mix(mut value: u64) -> u64 {
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

/// The cached random base of one call, in `[0, 1)`.
fn base_value(scope: &Scope<'_>, caching: Option<&Caching<'_>>, index: usize) -> f64 {
    let owner = scope.owner;
    let implicit = caching.is_none_or(|caching| caching.auto);
    let mut hash = fnv(&owner.element.map_or(0, |(_, node)| node.document_id()).to_le_bytes(), 0xcbf2_9ce4_8422_2325);
    if let Some(ident) = caching.and_then(|caching| caching.ident) {
        hash = fnv(ident.as_bytes(), hash);
    }
    hash = fnv(b"|", hash);
    if implicit || caching.is_some_and(|caching| caching.element) {
        hash = fnv(&owner.element.map_or(0, |(_, node)| node.key()).to_le_bytes(), hash);
    }
    hash = fnv(b"|", hash);
    let ua = if implicit { Some(Ua::PropertyIndex) } else { caching.and_then(|caching| caching.ua) };
    match ua {
        Some(Ua::Property) => hash = fnv(owner.property.as_bytes(), hash),
        Some(Ua::PropertyIndex) => {
            hash = fnv(owner.property.as_bytes(), hash);
            hash = fnv(&index.to_le_bytes(), hash);
        }
        Some(Ua::Named(name)) => hash = fnv(ascii_lower(name).as_bytes(), hash),
        None => {}
    }
    (mix(hash) >> 11) as f64 / (1u64 << 53) as f64
}

fn calc_sum(text: &str) -> Option<typed_numeric::NumericExpression> {
    let expression = typed_numeric::parse_numeric_expression(&alloc::format!("calc({})", text.trim()))?;
    Some(match expression {
        typed_numeric::NumericExpression::Calc(inner) => *inner,
        other => other,
    })
}

fn simple_value(text: &str) -> Option<NumericValue> {
    let mut expression = calc_sum(text)?;
    expression.simplify_absolute_units();
    let value = expression.single_numeric_value()?;
    Some(match value.unit.canonical_unit_and_factor() {
        Some((unit, factor)) => NumericValue { value: value.value * factor, unit },
        None => value,
    })
}

fn random_number(min: f64, max: f64, step: Option<f64>, base: f64) -> f64 {
    if min.is_nan() || max.is_nan() || step.is_some_and(f64::is_nan) {
        return f64::NAN;
    }
    if min.is_infinite() || max < min {
        return min;
    }
    if max.is_infinite() {
        return f64::NAN;
    }
    let range = max - min;
    match step.filter(|step| *step > 0.0) {
        Some(step) if step.is_infinite() => min,
        Some(step) => {
            let count = libm::floor(range / step) + 1.0;
            min + libm::floor(base * count).min(count - 1.0) * step
        }
        None => min + base * range,
    }
}

fn number_text(value: f64, unit: NumericUnit) -> String {
    if value.is_finite() {
        return alloc::format!("calc({})", typed_numeric::serialize_numeric_value(value, unit));
    }
    let constant = if value.is_nan() { "NaN" } else if value.is_sign_negative() { "-infinity" } else { "infinity" };
    if unit == NumericUnit::Number {
        alloc::format!("calc({constant})")
    } else {
        alloc::format!("calc({constant} * 1{})", typed_numeric::infinite_unit_text(unit))
    }
}

fn clamp_base(value: f64) -> f64 {
    if value.is_nan() || value < 0.0 {
        0.0
    } else {
        value.min(1.0 - 1.0 / (1u64 << 53) as f64)
    }
}

fn expand(raw: &str, scope: &Scope<'_>, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    scope.charge(raw.len())?;
    let calls = find_calls(raw)?;
    if calls.is_empty() {
        return Some(String::from(raw));
    }
    let mut out = String::with_capacity(raw.len());
    let mut last = 0;
    for call in &calls {
        out.push_str(&raw[last..call.start]);
        let text = inner(raw, call);
        let value = match call.kind {
            Kind::Random => expand_random(text, scope, depth)?,
            Kind::RandomItem => expand_random_item(text, scope, depth)?,
            Kind::Attr => expand_attr(text, scope, depth)?,
            Kind::Ident => expand_ident(text, scope, depth)?,
        };
        out.push_str(&value);
        last = call.after;
        if out.len() > MAX_VARIABLE_BYTES {
            return None;
        }
    }
    out.push_str(&raw[last..]);
    Some(out)
}

fn next_index(scope: &Scope<'_>) -> usize {
    let index = scope.index.get() + 1;
    scope.index.set(index);
    index
}

fn call_base(scope: &Scope<'_>, caching_text: Option<&str>, depth: usize, index: usize) -> Option<f64> {
    let Some(text) = caching_text else { return Some(base_value(scope, None, index)) };
    scope.charge(text.len())?;
    let caching = parse_caching(text)?;
    match caching.fixed {
        Some(fixed) => {
            let fixed = expand(fixed, scope, depth + 1)?;
            let value = simple_value(&fixed).filter(|value| value.unit == NumericUnit::Number)?;
            Some(clamp_base(value.value))
        }
        None => Some(base_value(scope, Some(&caching), index)),
    }
}

fn expand_random(text: &str, scope: &Scope<'_>, depth: usize) -> Option<String> {
    let index = next_index(scope);
    let arguments = random_arguments(text)?;
    let base = call_base(scope, arguments.caching, depth, index)?;
    let min = expand(arguments.min, scope, depth + 1)?;
    let max = expand(arguments.max, scope, depth + 1)?;
    let step = match arguments.step {
        Some(step) => Some(expand(step, scope, depth + 1)?),
        None => None,
    };
    let numeric = simple_value(&min).zip(simple_value(&max)).and_then(|(min, max)| {
        let step = match &step {
            Some(step) => Some(simple_value(step)?),
            None => None,
        };
        let same = min.unit == max.unit && step.is_none_or(|step| step.unit == min.unit);
        same.then(|| number_text(random_number(min.value, max.value, step.map(|step| step.value), base), min.unit))
    });
    if let Some(value) = numeric {
        return Some(value);
    }
    let range = alloc::format!("(({max}) - ({min}))");
    let spread = alloc::format!("{base} * max(0 * {range}, {range})");
    Some(match step {
        Some(step) => alloc::format!("calc(({min}) + round(down, {spread}, {step}))"),
        None => alloc::format!("calc(({min}) + {spread})"),
    })
}

fn expand_random_item(text: &str, scope: &Scope<'_>, depth: usize) -> Option<String> {
    let index = next_index(scope);
    let arguments = random_item_arguments(text)?;
    let base = call_base(scope, Some(arguments.caching.trim()), depth, index)?;
    let count = arguments.items.len();
    let chosen = (libm::floor(base * count as f64) as usize).min(count - 1);
    expand(arguments.items[chosen], scope, depth + 1)
}

enum AttrType<'a> {
    Text,
    Syntax(&'a str),
    Unit(&'a str),
}

struct AttrHead<'a> {
    name: &'a str,
    namespaced: bool,
    kind: AttrType<'a>,
}

fn attr_head(head: &str) -> Option<AttrHead<'_>> {
    let tokens = components(head)?;
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    let (namespace, name) = match tokens[0].rsplit_once('|') {
        Some((namespace, name)) => (Some(namespace), name),
        None => (None, tokens[0]),
    };
    let identifier = |text: &str| {
        let mut at = 0;
        consume_selector_identifier(text, &mut at).is_some_and(|_| at == text.len())
    };
    if !identifier(name) || namespace.is_some_and(|namespace| namespace != "*" && !namespace.is_empty() && !identifier(namespace)) {
        return None;
    }
    let namespaced = namespace.is_some_and(|namespace| !namespace.is_empty());
    let kind = match tokens.get(1) {
        None => AttrType::Text,
        Some(token) if token.len() > 6 && token.as_bytes()[..5].eq_ignore_ascii_case(b"type(") && token.ends_with(')') => {
            let syntax = token[5..token.len() - 1].trim();
            if syntax != "*" && !registered_properties::valid_typed_syntax(syntax) {
                return None;
            }
            AttrType::Syntax(syntax)
        }
        Some(token) if token.eq_ignore_ascii_case("raw-string") => AttrType::Text,
        Some(token) if *token == "%" || identifier(token) => AttrType::Unit(token),
        Some(_) => return None,
    };
    Some(AttrHead { name, namespaced, kind })
}

fn css_string_literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            '\n' => out.push_str("\\a "),
            '\r' => out.push_str("\\d "),
            '\x0c' => out.push_str("\\c "),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn plain_number(text: &str) -> bool {
    let text = text.trim();
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E'))
        && text.parse::<f64>().is_ok_and(f64::is_finite)
}

enum Attribute {
    Value(String),
    Missing,
    Invalid,
}

fn attribute_value<'a>(scope: &'a Scope<'_>, head: &AttrHead<'_>) -> Option<&'a str> {
    if head.namespaced {
        return None;
    }
    let (document, node) = scope.owner.element?;
    let exact = document.get_attribute_ns_ref(node, None, head.name).ok().flatten();
    let html = document.is_html_document()
        && matches!(document.kind(node), Ok(crate::NodeKind::Element { namespace: crate::Namespace::Html, .. }));
    exact.or_else(|| if html { document.get_attribute_ns_ref(node, None, &ascii_lower(head.name)).ok().flatten() } else { None })
}

fn resolve_attr(scope: &Scope<'_>, head: &AttrHead<'_>, depth: usize) -> Attribute {
    let Some(value) = attribute_value(scope, head) else {
        return Attribute::Missing;
    };
    match head.kind {
        AttrType::Text => Attribute::Value(css_string_literal(value)),
        AttrType::Unit(unit) => {
            if !plain_number(value) {
                return Attribute::Missing;
            }
            let text = alloc::format!("{}{unit}", value.trim());
            if typed_numeric::parse_numeric_value(&text).is_some() { Attribute::Value(text) } else { Attribute::Missing }
        }
        AttrType::Syntax(syntax) => {
            let value = value.trim();
            if syntax == "*" {
                if scope.attributes.borrow().iter().any(|name| name == head.name) {
                    return Attribute::Invalid;
                }
                scope.attributes.borrow_mut().push(String::from(head.name));
                let expanded = expand(value, scope, depth + 1);
                scope.attributes.borrow_mut().pop();
                return match expanded {
                    Some(text) => Attribute::Value(text),
                    None => Attribute::Missing,
                };
            }
            if !registered_properties::accepts_value(syntax, value) {
                return Attribute::Missing;
            }
            let canonical = if value.contains('(') { None } else { typed_numeric::parse_numeric_value(value) };
            Attribute::Value(match canonical {
                Some(number) => typed_numeric::serialize_numeric_value(number.value, number.unit),
                None => String::from(value),
            })
        }
    }
}

fn expand_attr(text: &str, scope: &Scope<'_>, depth: usize) -> Option<String> {
    let (head, fallback) = match syntax::top_level_find(text, b',')? {
        Some(at) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    };
    let head = attr_head(head)?;
    match resolve_attr(scope, &head, depth) {
        Attribute::Value(value) => Some(value),
        Attribute::Invalid => None,
        Attribute::Missing => match fallback {
            Some(fallback) => expand(fallback.trim(), scope, depth + 1),
            None if matches!(head.kind, AttrType::Text) => Some(css_string_literal("")),
            None => None,
        },
    }
}

fn ident_arguments(inner: &str) -> Option<Vec<&str>> {
    if syntax::top_level_find(inner, b',')?.is_some() {
        return None;
    }
    let tokens = components(inner)?;
    (!tokens.is_empty() && tokens.iter().all(|token| !token.starts_with('{'))).then_some(tokens)
}

fn expand_ident(text: &str, scope: &Scope<'_>, depth: usize) -> Option<String> {
    let text = expand(text, scope, depth + 1)?;
    let tokens = ident_arguments(&text)?;
    let (position, count) = match scope.owner.element {
        Some((document, node)) => element_sibling_position(document, node),
        None => (1, 1),
    };
    let mut name = String::new();
    for token in tokens {
        if token.starts_with(['"', '\'']) {
            name.push_str(&css_string(token)?);
            continue;
        }
        let mut at = 0;
        if let Some(part) = consume_selector_identifier(token, &mut at).filter(|_| at == token.len()) {
            name.push_str(&part);
            continue;
        }
        let (resolved, _) = typed_numeric::substitute_sibling_functions(token, position, count)?;
        let value = simple_value(&resolved).filter(|value| value.unit == NumericUnit::Number)?.value;
        if !value.is_finite() || value.fract() != 0.0 {
            return None;
        }
        name.push_str(&alloc::format!("{}", value as i64));
    }
    (!name.is_empty()).then(|| identifier::serialize_identifier(&name))
}

/// The text of `raw` with its element-dependent functions evaluated for `owner`; `None` is
/// invalid at computed-value time.
pub(super) fn substitute(raw: &str, owner: &Owner<'_>) -> Option<String> {
    let scope = Scope::new(owner);
    expand(raw, &scope, 0)
}

/// `raw` with its element-dependent functions evaluated for `node`, with a lone `calc(<value>)`
/// result written as the plain value; `None` when `raw` uses none of them or is invalid.
pub(super) fn substitute_for_element(raw: &str, document: &Document, node: NodeId, property: &str) -> Option<String> {
    if !may_use(raw) {
        return None;
    }
    let text = substitute(raw, &Owner { element: Some((document, node)), property })?;
    let trimmed = text.trim();
    let plain = trimmed
        .get(..5)
        .filter(|prefix| prefix.eq_ignore_ascii_case("calc("))
        .and_then(|_| trimmed.strip_suffix(')'))
        .and_then(|inner| typed_numeric::parse_numeric_value(&inner[5..]))
        .map(|value| typed_numeric::serialize_numeric_value(value.value, value.unit));
    Some(plain.unwrap_or(text))
}

/// What a declaration using these functions is validated against when it is parsed.
pub(super) enum Admission {
    Invalid,
    Unchecked,
    Probe(String),
}

fn probe(raw: &str, depth: usize) -> Option<Admission> {
    if depth > MAX_DEPTH {
        return None;
    }
    let calls = find_calls(raw)?;
    let mut out = String::with_capacity(raw.len());
    let mut last = 0;
    let mut checked = true;
    for call in &calls {
        out.push_str(&raw[last..call.start]);
        let text = inner(raw, call);
        match call.kind {
            Kind::Attr => {
                let head = match syntax::top_level_find(text, b',')? {
                    Some(at) => &text[..at],
                    None => text,
                };
                attr_head(head)?;
                checked = false;
                out.push_str("0");
            }
            Kind::RandomItem => {
                random_item_arguments(text)?;
                checked = false;
                out.push_str("0");
            }
            Kind::Ident => {
                ident_arguments(text)?;
                checked = false;
                out.push_str("0");
            }
            Kind::Random => {
                let arguments = random_arguments(text)?;
                if let Some(caching) = arguments.caching {
                    parse_caching(caching)?;
                }
                let mut parts = Vec::new();
                for part in [Some(arguments.min), Some(arguments.max), arguments.step].into_iter().flatten() {
                    match probe(part, depth + 1)? {
                        Admission::Probe(text) => parts.push(text),
                        Admission::Unchecked => return Some(Admission::Unchecked),
                        Admission::Invalid => return None,
                    }
                }
                out.push_str(&alloc::format!("calc(({}))", parts.join(") + (")));
            }
        }
        last = call.after;
    }
    out.push_str(&raw[last..]);
    Some(if checked { Admission::Probe(out) } else { Admission::Unchecked })
}

pub(super) fn admission(raw: &str) -> Admission {
    probe(raw, 0).unwrap_or(Admission::Invalid)
}

fn argument_text(text: &str, scope: &Scope<'_>) -> Option<String> {
    if may_use(text) {
        return canonical_text(text, scope);
    }
    let explicit = text.trim().get(..5).is_some_and(|prefix| prefix.eq_ignore_ascii_case("calc("));
    let mut expression = calc_sum(text)?;
    expression.simplify_absolute_units();
    if !explicit {
        if let Some(value) = expression.single_numeric_value() {
            return Some(typed_numeric::serialize_numeric_value(value.value, value.unit));
        }
    }
    expression.serialize()
}

fn canonical_random(text: &str, scope: &Scope<'_>) -> Option<String> {
    let index = next_index(scope);
    let arguments = random_arguments(text)?;
    let property = scope.owner.property;
    let caching = match arguments.caching {
        Some(text) => Some(parse_caching(text)?),
        None => None,
    };
    let args: Vec<&str> = [Some(arguments.min), Some(arguments.max), arguments.step].into_iter().flatten().collect();
    let mut texts = Vec::with_capacity(args.len());
    for arg in &args {
        texts.push(argument_text(arg, scope)?);
    }
    let implicit = caching.as_ref().is_none_or(|caching| caching.auto);
    let mut leading = String::new();
    let mut push = |text: &str| {
        if !leading.is_empty() {
            leading.push(' ');
        }
        leading.push_str(text);
    };
    if let Some(fixed) = caching.as_ref().and_then(|caching| caching.fixed) {
        let base = clamp_base(simple_value(fixed).filter(|value| value.unit == NumericUnit::Number)?.value);
        let values: Option<Vec<NumericValue>> = texts.iter().map(|text| simple_value(text)).collect();
        if let Some(values) = values {
            let same = values.iter().all(|value| value.unit == values[0].unit);
            if same {
                let step = values.get(2).map(|value| value.value);
                return Some(number_text(random_number(values[0].value, values[1].value, step, base), values[0].unit));
            }
        }
        let fixed = argument_text(fixed, scope)?;
        push(&alloc::format!("fixed {fixed}"));
    } else if implicit {
        push("element-scoped");
        push(&alloc::format!("ua-{property}-{index}"));
    } else if let Some(caching) = &caching {
        if let Some(ident) = caching.ident {
            push(ident);
        }
        if caching.element {
            push("element-scoped");
        }
        match caching.ua {
            Some(Ua::Property) => push(&alloc::format!("ua-{property}")),
            Some(Ua::PropertyIndex) => push(&alloc::format!("ua-{property}-{index}")),
            Some(Ua::Named(name)) => push(name),
            None => {}
        }
    }
    Some(alloc::format!("random({leading}, {})", texts.join(", ")))
}

fn canonical_text(raw: &str, scope: &Scope<'_>) -> Option<String> {
    let calls = find_calls(raw)?;
    let mut out = String::with_capacity(raw.len());
    let mut last = 0;
    for call in &calls {
        out.push_str(&raw[last..call.start]);
        if call.kind == Kind::Random {
            out.push_str(&canonical_random(inner(raw, call), scope)?);
        } else {
            out.push_str(&raw[call.start..call.after]);
        }
        last = call.after;
    }
    out.push_str(&raw[last..]);
    Some(out)
}

/// Specified-value serialization of a declaration using `random()`: explicit cache keys and
/// simplified arguments; other calls are kept as written.
pub(super) fn specified(property: &str, raw: &str) -> Option<String> {
    let calls = find_calls(raw)?;
    if !calls.iter().any(|call| call.kind == Kind::Random) {
        return None;
    }
    let owner = Owner { element: None, property };
    let scope = Scope::new(&owner);
    canonical_text(raw, &scope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_outermost_calls_outside_strings() {
        let calls = find_calls("calc(2 * random(--a, 0px, 1px)) \"attr(x)\" attr(data-x)").unwrap();
        assert_eq!(calls.len(), 2);
        assert!(find_calls("random(1, 2").is_none());
    }

    #[test]
    fn random_number_follows_range_and_step() {
        assert_eq!(random_number(100.0, 10.0, None, 0.5), 100.0);
        assert_eq!(random_number(0.0, 100.0, Some(3.0), 0.5), 51.0);
        assert!(random_number(10.0, f64::INFINITY, None, 0.5).is_nan());
        assert_eq!(random_number(10.0, 100.0, Some(f64::INFINITY), 0.9), 10.0);
    }

    #[test]
    fn caching_options_reject_conflicts() {
        assert!(parse_caching("--a element-scoped ua-x").is_some());
        assert!(parse_caching("property-scoped property-index-scoped").is_none());
        assert!(parse_caching("fixed -1").is_none());
        assert!(parse_caching("element-scoped uawidth").is_none());
    }
}
