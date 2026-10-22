//! CSS Anchor Positioning grammar: the `anchor()` and `anchor-size()`
//! functions inside inset, margin and sizing values, and the specified and
//! computed forms of the anchor positioning properties.
use super::typed_numeric::{self, NumericExpression, NumericType, NumericUnit};
use super::*;

/// Properties introduced by this specification, in `extra_props` order.
pub(crate) const PROPERTIES: [&str; 7] = [
    "anchor-name",
    "anchor-scope",
    "position-anchor",
    "position-area",
    "position-try-fallbacks",
    "position-try-order",
    "position-visibility",
];

pub(crate) fn initial(property: &str) -> &'static str {
    match property {
        "position-anchor" | "position-try-order" => "normal",
        "position-visibility" => "anchor-visible",
        _ => "none",
    }
}

/// Which functions a property admits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    Inset,
    Margin,
    Size,
}

fn context_of(name: &str) -> Option<Context> {
    Some(match name {
        "top" | "right" | "bottom" | "left" | "inset" | "inset-block" | "inset-inline" | "inset-block-start"
        | "inset-block-end" | "inset-inline-start" | "inset-inline-end" => Context::Inset,
        "margin" | "margin-top" | "margin-right" | "margin-bottom" | "margin-left" | "margin-block"
        | "margin-inline" | "margin-block-start" | "margin-block-end" | "margin-inline-start"
        | "margin-inline-end" => Context::Margin,
        "width" | "height" | "min-width" | "min-height" | "max-width" | "max-height" | "inline-size"
        | "block-size" | "min-inline-size" | "min-block-size" | "max-inline-size" | "max-block-size" => Context::Size,
        _ => return None,
    })
}

fn component_limit(name: &str) -> usize {
    match name {
        "inset" | "margin" => 4,
        "inset-block" | "inset-inline" | "margin-block" | "margin-inline" => 2,
        _ => 1,
    }
}

fn longhand_names(name: &str) -> &'static [&'static str] {
    match name {
        "inset" => &["top", "right", "bottom", "left"],
        "margin" => &["margin-top", "margin-right", "margin-bottom", "margin-left"],
        "inset-block" => &["inset-block-start", "inset-block-end"],
        "inset-inline" => &["inset-inline-start", "inset-inline-end"],
        "margin-block" => &["margin-block-start", "margin-block-end"],
        "margin-inline" => &["margin-inline-start", "margin-inline-end"],
        _ => &[],
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'\\' || syntax::is_name_byte(byte)
}

fn function_at(raw: &str, at: usize) -> Option<&'static str> {
    let bytes = raw.as_bytes();
    if at > 0 && is_ident_byte(bytes[at - 1]) {
        return None;
    }
    [("anchor-size", 11usize), ("anchor", 6usize)].into_iter().find_map(|(name, length)| {
        let name_end = at.checked_add(length)?;
        let head = bytes.get(at..name_end)?;
        (head.eq_ignore_ascii_case(name.as_bytes()) && bytes.get(name_end) == Some(&b'(')).then_some(name)
    })
}

/// Whether `raw` contains an `anchor()` or `anchor-size()` function.
pub(super) fn mentions(raw: &str) -> bool {
    (0..raw.len()).any(|at| raw.is_char_boundary(at) && function_at(raw, at).is_some())
}

/// Whether the declaration is an inset, margin or size value using anchor functions.
pub(super) fn applies(name: &str, raw: &str) -> bool {
    context_of(name).is_some() && mentions(raw)
}

/// Units that cannot appear in an anchor-free computed expression stand in for
/// each function while the shared math grammar types, simplifies and orders
/// the expression. They are sorted so term order survives the serializer.
const PLACEHOLDER_UNITS: [&str; 24] = [
    "cqb", "cqh", "cqi", "cqmax", "cqmin", "cqw", "dvb", "dvh", "dvi", "dvmax", "dvmin", "dvw", "lvb", "lvh", "lvi",
    "lvmax", "lvmin", "lvw", "svb", "svh", "svi", "svmax", "svmin", "svw",
];

struct Table {
    units: Vec<&'static str>,
    texts: Vec<String>,
}

impl Table {
    fn new(source: &str) -> Self {
        let lower = source.to_ascii_lowercase();
        Table {
            units: PLACEHOLDER_UNITS.iter().copied().filter(|unit| !lower.contains(unit)).collect(),
            texts: Vec::new(),
        }
    }

    fn add(&mut self, text: String) -> Option<String> {
        let unit = *self.units.get(self.texts.len())?;
        self.texts.push(text);
        Some(alloc::format!("1{unit}"))
    }

    fn text_of(&self, unit: &str) -> Option<&str> {
        let index = self.units.iter().position(|candidate| *candidate == unit)?;
        self.texts.get(index).map(String::as_str)
    }

    fn owns(&self, unit: NumericUnit) -> bool {
        self.text_of(unit.as_str()).is_some()
    }
}

const MAX_DEPTH: usize = 8;

fn substitute(raw: &str, table: &mut Table, allow_anchor: bool, depth: usize) -> Option<String> {
    let mut out = String::new();
    let mut at = 0;
    while at < raw.len() {
        if let Some(name) = function_at(raw, at) {
            if name == "anchor" && !allow_anchor {
                return None;
            }
            let open = at + name.len();
            let end = matching_css_block(raw, open)?;
            let text = canonical_function(name, &raw[open + 1..end - 1], allow_anchor, depth + 1)?;
            out.push_str(&table.add(text)?);
            at = end;
            continue;
        }
        let character = raw[at..].chars().next()?;
        if matches!(character, '"' | '\'') {
            return None;
        }
        out.push(character);
        at += character.len_utf8();
    }
    Some(out)
}

/// The serialized `<dashed-ident>` naming a `@position-try` rule.
pub fn position_try_name(prelude: &str) -> Option<String> {
    dashed_ident(nesting::at_rule_tail(prelude.trim(), "@position-try")?.trim())
}

/// Properties a `@position-try` rule accepts.
pub fn position_try_property(name: &str) -> bool {
    context_of(name).is_some()
        || matches!(name, "place-self" | "align-self" | "justify-self" | "position-anchor" | "position-area")
}

fn dashed_ident(token: &str) -> Option<String> {
    let mut position = 0;
    let identifier = consume_selector_identifier(token, &mut position)?;
    (position == token.len() && identifier.len() > 2 && identifier.starts_with("--"))
        .then(|| serialize_css_identifier(&identifier))
}

fn length_percentage_type(ty: NumericType) -> bool {
    ty.angle == 0
        && ty.time == 0
        && ty.frequency == 0
        && ty.resolution == 0
        && ty.flex == 0
        && ((ty.length == 1 && ty.percent == 0) || (ty.length == 0 && ty.percent == 1))
}

fn percentage_type(ty: NumericType) -> bool {
    ty.length == 0 && ty.angle == 0 && ty.time == 0 && ty.frequency == 0 && ty.resolution == 0 && ty.flex == 0 && ty.percent == 1
}

fn anchor_side(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "inside" | "outside" | "top" | "left" | "right" | "bottom" | "start" | "end" | "self-start" | "self-end" | "center"
    ) {
        return Some(lower);
    }
    let expression = typed_numeric::parse_numeric_expression(token)?;
    percentage_type(expression.numeric_type()?).then_some(())?;
    expression.serialize_specified()
}

fn anchor_size_keyword(token: &str) -> Option<String> {
    let lower = token.to_ascii_lowercase();
    matches!(lower.as_str(), "width" | "height" | "block" | "inline" | "self-block" | "self-inline").then_some(lower)
}

fn canonical_function(name: &str, inner: &str, allow_anchor: bool, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return None;
    }
    let parts = top_level_split(inner, b',', 2)?;
    let head = parts[0].trim();
    let fallback = parts.get(1).map(|part| part.trim());
    let mut text = String::from(name);
    text.push('(');
    if name == "anchor" {
        let tokens = components(head)?;
        if tokens.is_empty() || tokens.len() > 2 {
            return None;
        }
        let (mut anchor_name, mut side) = (None, None);
        for token in tokens {
            if token.starts_with("--") {
                if anchor_name.replace(dashed_ident(token)?).is_some() {
                    return None;
                }
            } else if side.replace(anchor_side(token)?).is_some() {
                return None;
            }
        }
        if let Some(anchor_name) = anchor_name {
            text.push_str(&anchor_name);
            text.push(' ');
        }
        text.push_str(&side?);
    } else if head.is_empty() {
        if fallback.is_some() {
            return None;
        }
    } else {
        let tokens = components(head)?;
        if tokens.len() > 2 {
            return None;
        }
        if tokens.len() == 1
            && fallback.is_none()
            && !tokens[0].starts_with("--")
            && anchor_size_keyword(tokens[0]).is_none()
        {
            text.push_str(&length_percentage(tokens[0], allow_anchor, depth)?);
            text.push(')');
            return Some(text);
        }
        let (mut anchor_name, mut size) = (None, None);
        for token in tokens {
            if token.starts_with("--") {
                if anchor_name.replace(dashed_ident(token)?).is_some() {
                    return None;
                }
            } else if size.replace(anchor_size_keyword(token)?).is_some() {
                return None;
            }
        }
        if let Some(anchor_name) = &anchor_name {
            text.push_str(anchor_name);
        }
        if let Some(size) = size {
            if anchor_name.is_some() {
                text.push(' ');
            }
            text.push_str(&size);
        }
    }
    if let Some(fallback) = fallback {
        if fallback.is_empty() {
            return None;
        }
        text.push_str(", ");
        text.push_str(&length_percentage(fallback, allow_anchor, depth)?);
    }
    text.push(')');
    Some(text)
}

fn unwrap_single_function(expression: &mut NumericExpression, table: &Table) {
    match expression {
        NumericExpression::Calc(inner) | NumericExpression::Negate(inner) | NumericExpression::Invert(inner) | NumericExpression::Sign(inner) => {
            unwrap_single_function(inner, table)
        }
        NumericExpression::Sum(values) | NumericExpression::Product(values) | NumericExpression::Min(values) | NumericExpression::Max(values) => {
            for value in values.iter_mut() {
                unwrap_single_function(value, table);
            }
        }
        NumericExpression::Function(_, values) => {
            for value in values.iter_mut() {
                unwrap_single_function(value, table);
            }
        }
        NumericExpression::Clamp(lower, value, upper) => {
            unwrap_single_function(lower, table);
            unwrap_single_function(value, table);
            unwrap_single_function(upper, table);
        }
        NumericExpression::Value(_) | NumericExpression::Identifier(_) => {}
    }
    let replacement = match &*expression {
        NumericExpression::Calc(inner) => match &**inner {
            NumericExpression::Value(value) if value.value == 1.0 && table.owns(value.unit) => {
                Some(NumericExpression::Value(*value))
            }
            _ => None,
        },
        _ => None,
    };
    if let Some(replacement) = replacement {
        *expression = replacement;
    }
}

fn restore(serialized: &str, table: &Table) -> Option<String> {
    let bytes = serialized.as_bytes();
    let mut out = String::new();
    let mut at = 0;
    while at < bytes.len() {
        let starts_number = bytes[at].is_ascii_digit() || (bytes[at] == b'.' && bytes.get(at + 1).is_some_and(u8::is_ascii_digit));
        if !starts_number {
            let character = serialized[at..].chars().next()?;
            out.push(character);
            at += character.len_utf8();
            continue;
        }
        let start = at;
        while at < bytes.len() && (bytes[at].is_ascii_digit() || bytes[at] == b'.') {
            at += 1;
        }
        if at < bytes.len()
            && bytes[at] == b'e'
            && (bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
                || (matches!(bytes.get(at + 1), Some(b'-' | b'+')) && bytes.get(at + 2).is_some_and(u8::is_ascii_digit)))
        {
            at += 2;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
        }
        let number = &serialized[start..at];
        let unit_start = at;
        while at < bytes.len() && bytes[at].is_ascii_alphabetic() {
            at += 1;
        }
        match table.text_of(&serialized[unit_start..at]) {
            Some(text) => {
                if number.parse::<f64>().ok()? == 1.0 {
                    out.push_str(text);
                } else {
                    out.push_str(number);
                    out.push_str(" * ");
                    out.push_str(text);
                }
            }
            None => out.push_str(&serialized[start..at]),
        }
    }
    Some(out)
}

/// The canonical serialization of one `<length-percentage>` that may contain
/// anchor functions, or `None` when it is not valid.
fn length_percentage(text: &str, allow_anchor: bool, depth: usize) -> Option<String> {
    let mut table = Table::new(text);
    let substituted = substitute(text, &mut table, allow_anchor, depth)?;
    let substituted = substituted.trim();
    if substituted.is_empty() {
        return None;
    }
    let mut expression = typed_numeric::parse_numeric_expression(substituted)?;
    if let NumericExpression::Value(value) = &expression {
        if value.unit == NumericUnit::Number {
            return (value.value == 0.0).then(|| String::from("0px"));
        }
    }
    length_percentage_type(expression.numeric_type()?).then_some(())?;
    unwrap_single_function(&mut expression, &table);
    restore(&expression.serialize_specified()?, &table)
}

fn plain_component_valid(name: &str, part: &str) -> bool {
    let longhand = longhand_names(name).first().copied().unwrap_or(name);
    declarations_with_variables(&alloc::format!("{longhand}:{part}"), 0, false).is_ok_and(|declarations| !declarations.is_empty())
}

/// The canonical specified value of `name: raw` when it uses anchor functions;
/// `None` is returned for values this module does not own.
pub(super) fn canonicalize(name: &str, raw: &str) -> Option<Option<String>> {
    applies(name, raw).then(|| canonical_value(name, raw))
}

fn canonical_value(name: &str, raw: &str) -> Option<String> {
    let context = context_of(name)?;
    let parts = components(raw)?;
    if parts.is_empty() || parts.len() > component_limit(name) {
        return None;
    }
    let mut canonical = Vec::new();
    for part in parts {
        if mentions(part) {
            canonical.push(length_percentage(part, context == Context::Inset, 0)?);
        } else if plain_component_valid(name, part) {
            canonical.push(part.to_owned());
        } else {
            return None;
        }
    }
    Some(match (name, canonical.as_slice()) {
        ("inset" | "margin", [a]) => a.clone(),
        ("inset" | "margin", [a, b]) => serialize_box_sides_owned([a, b, a, b]),
        ("inset" | "margin", [a, b, c]) => serialize_box_sides_owned([a, b, c, b]),
        ("inset" | "margin", [a, b, c, d]) => serialize_box_sides_owned([a, b, c, d]),
        (_, [a, b]) if a == b => a.clone(),
        _ => canonical.join(" "),
    })
}

fn serialize_box_sides_owned(sides: [&String; 4]) -> String {
    declaration_block::serialize_box_sides([sides[0], sides[1], sides[2], sides[3]])
}

fn expand_sides(name: &str, canonical: &str) -> Option<Vec<(String, String)>> {
    let parts = components(canonical)?;
    let longhands = longhand_names(name);
    if longhands.is_empty() {
        return Some(vec![(name.to_owned(), canonical.to_owned())]);
    }
    longhands
        .iter()
        .enumerate()
        .map(|(index, longhand)| {
            let source = match (longhands.len(), parts.len()) {
                (_, 1) => 0,
                (4, 2) => index % 2,
                (4, 3) => [0, 1, 2, 1][index],
                (count, given) if count == given => index,
                _ => return None,
            };
            Some(((*longhand).to_owned(), (*parts.get(source)?).to_owned()))
        })
        .collect()
}

/// Parse `name: raw`, using anchor functions, into declarations. While
/// `defer` is set the declaration is kept as text for the cascade expansion.
pub(super) fn declarations(name: &str, raw: &str, important: bool, defer: bool, out: &mut Vec<Declaration>) {
    let Some(canonical) = canonical_value(name, raw) else {
        return;
    };
    if defer {
        out.push(Declaration { value: Value::Deferred(name.into(), canonical.into()), important });
        return;
    }
    let Some(sides) = expand_sides(name, &canonical) else {
        return;
    };
    for (longhand, part) in sides {
        if mentions(&part) {
            if let Some(&slot) = slots(&longhand).first() {
                out.push(Declaration { value: Value::Anchor(slot, Arc::from(alloc::format!("{longhand}:{part}"))), important });
            }
        } else if let Ok(parsed) = declarations_with_variables(&alloc::format!("{longhand}:{part}"), 0, false) {
            out.extend(parsed.into_iter().map(|declaration| Declaration { value: declaration.value, important }));
        }
    }
}

impl Style {
    pub(super) fn set_anchored(&mut self, slot: usize, raw: Arc<str>) {
        let mut entries: Vec<(usize, Arc<str>)> = self.anchored.as_deref().map(<[_]>::to_vec).unwrap_or_default();
        entries.retain(|(existing, _)| *existing != slot);
        entries.push((slot, raw));
        self.anchored = Some(Arc::from(entries));
    }
}

// ---------------------------------------------------------------------
// Anchor positioning properties
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
    Block,
    Inline,
    SelfBlock,
    SelfInline,
    Start,
    SelfStart,
    Any,
}

fn classify(token: &str) -> Option<Axis> {
    if matches!(token, "center" | "span-all") {
        return Some(Axis::Any);
    }
    Some(match token.strip_prefix("span-").unwrap_or(token) {
        "left" | "right" | "x-start" | "x-end" | "self-x-start" | "self-x-end" | "x-self-start" | "x-self-end" => Axis::Horizontal,
        "top" | "bottom" | "y-start" | "y-end" | "self-y-start" | "self-y-end" | "y-self-start" | "y-self-end" => Axis::Vertical,
        "block-start" | "block-end" => Axis::Block,
        "inline-start" | "inline-end" => Axis::Inline,
        "self-block-start" | "self-block-end" => Axis::SelfBlock,
        "self-inline-start" | "self-inline-end" => Axis::SelfInline,
        "start" | "end" => Axis::Start,
        "self-start" | "self-end" => Axis::SelfStart,
        _ => return None,
    })
}

fn compatible(a: Axis, b: Axis) -> bool {
    matches!(
        (a, b),
        (Axis::Any, _)
            | (_, Axis::Any)
            | (Axis::Horizontal, Axis::Vertical)
            | (Axis::Vertical, Axis::Horizontal)
            | (Axis::Block, Axis::Inline)
            | (Axis::Inline, Axis::Block)
            | (Axis::SelfBlock, Axis::SelfInline)
            | (Axis::SelfInline, Axis::SelfBlock)
            | (Axis::Start, Axis::Start)
            | (Axis::SelfStart, Axis::SelfStart)
    )
}

/// The specified `<position-area>` tokens: one keyword, or two in canonical order.
fn area_tokens(raw: &str) -> Option<Vec<String>> {
    let lower = raw.to_ascii_lowercase();
    let tokens = components(&lower)?;
    let classes = tokens.iter().map(|token| classify(token)).collect::<Option<Vec<_>>>()?;
    match (tokens.as_slice(), classes.as_slice()) {
        ([one], _) => Some(vec![(*one).to_owned()]),
        ([a, b], [ca, cb]) => {
            if !compatible(*ca, *cb) {
                return None;
            }
            if a == b {
                return Some(vec![(*a).to_owned()]);
            }
            let pair = |first: &str, second: &str| Some(vec![first.to_owned(), second.to_owned()]);
            match (*ca, *cb) {
                (Axis::Horizontal, Axis::Vertical)
                | (Axis::Block, Axis::Inline)
                | (Axis::SelfBlock, Axis::SelfInline)
                | (Axis::Any, Axis::Any)
                | (Axis::Start, Axis::Start)
                | (Axis::SelfStart, Axis::SelfStart)
                | (Axis::Start | Axis::SelfStart, Axis::Any)
                | (Axis::Any, Axis::Start | Axis::SelfStart) => pair(a, b),
                (Axis::Vertical, Axis::Horizontal) | (Axis::Inline, Axis::Block) | (Axis::SelfInline, Axis::SelfBlock) => pair(b, a),
                (axis, Axis::Any) => lone_axis(a, axis, b),
                (Axis::Any, axis) => lone_axis(b, axis, a),
                _ => None,
            }
        }
        _ => None,
    }
}

/// An axis keyword combined with `center` or `span-all`.
fn lone_axis(keyword: &str, axis: Axis, other: &str) -> Option<Vec<String>> {
    if other == "span-all" {
        return Some(vec![keyword.to_owned()]);
    }
    if matches!(axis, Axis::Horizontal | Axis::Block | Axis::SelfBlock) {
        Some(vec![keyword.to_owned(), other.to_owned()])
    } else {
        Some(vec![other.to_owned(), keyword.to_owned()])
    }
}

fn reduce_logical(token: &str) -> String {
    token.replacen("block-", "", 1).replacen("inline-", "", 1)
}

fn with_axis(token: &str, axis: &str) -> String {
    let (span, rest) = match token.strip_prefix("span-") {
        Some(rest) => ("span-", rest),
        None => ("", token),
    };
    match rest.strip_prefix("self-") {
        Some(rest) => alloc::format!("{span}self-{axis}-{rest}"),
        None => alloc::format!("{span}{axis}-{rest}"),
    }
}

fn position_area(raw: &str, computed: bool) -> Option<String> {
    let tokens = area_tokens(raw)?;
    if !computed {
        return Some(tokens.join(" "));
    }
    if let [a, b] = tokens.as_slice() {
        let (ca, cb) = (classify(a)?, classify(b)?);
        let axes = match (ca, cb) {
            (Axis::Block, Axis::Inline) | (Axis::SelfBlock, Axis::SelfInline) => Some((reduce_logical(a), reduce_logical(b))),
            (Axis::Block | Axis::SelfBlock, Axis::Any) => Some((reduce_logical(a), b.clone())),
            (Axis::Any, Axis::Inline | Axis::SelfInline) => Some((a.clone(), reduce_logical(b))),
            _ => None,
        };
        if let Some((first, second)) = axes {
            return Some(if first == second { first } else { alloc::format!("{first} {second}") });
        }
        if matches!(ca, Axis::Start | Axis::SelfStart) && b == "span-all" {
            return Some(with_axis(a, "block"));
        }
        if matches!(cb, Axis::Start | Axis::SelfStart) && a == "span-all" {
            return Some(with_axis(b, "inline"));
        }
    }
    Some(tokens.join(" "))
}

fn name_list(raw: &str, keywords: &[&str]) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    if keywords.contains(&lower.as_str()) {
        return Some(lower);
    }
    let items = top_level_split(trimmed, b',', 64)?;
    let names = items.into_iter().map(dashed_ident).collect::<Option<Vec<_>>>()?;
    Some(names.join(", "))
}

const TACTICS: [&str; 5] = ["flip-block", "flip-inline", "flip-start", "flip-x", "flip-y"];

fn fallback_item(item: &str) -> Option<String> {
    let lower = item.to_ascii_lowercase();
    let tokens = components(&lower)?;
    if tokens.is_empty() {
        return None;
    }
    let tactic_or_name = tokens.iter().all(|token| token.starts_with("--") || TACTICS.contains(token));
    if !tactic_or_name {
        return position_area(item, false);
    }
    let mut name = None;
    let mut tactics: Vec<&str> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if token.starts_with("--") {
            if index != 0 && index + 1 != tokens.len() {
                return None;
            }
            let original = components(item)?.get(index).copied()?;
            if name.replace(dashed_ident(original)?).is_some() {
                return None;
            }
        } else if tactics.contains(token) {
            return None;
        } else {
            tactics.push(token);
        }
    }
    let mut parts = Vec::new();
    parts.extend(name);
    parts.extend(tactics.into_iter().map(str::to_owned));
    Some(parts.join(" "))
}

fn position_try_fallbacks(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("none") {
        return Some(String::from("none"));
    }
    let items = top_level_split(trimmed, b',', 33)?;
    let items = items.into_iter().map(fallback_item).collect::<Option<Vec<_>>>()?;
    Some(items.join(", "))
}

fn position_visibility(raw: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let tokens = components(&lower)?;
    if tokens.len() == 1 && tokens[0] == "always" {
        return Some(String::from("always"));
    }
    let known = ["anchor-valid", "anchor-visible", "no-overflow"];
    let mut seen = [false; 3];
    for token in &tokens {
        let index = known.iter().position(|candidate| candidate == token)?;
        if seen[index] {
            return None;
        }
        seen[index] = true;
    }
    if tokens.is_empty() {
        return None;
    }
    Some(known.iter().zip(seen).filter(|(_, present)| *present).map(|(token, _)| *token).collect::<Vec<_>>().join(" "))
}

fn specified_or_computed(property: &str, raw: &str, computed: bool) -> Option<String> {
    let raw = raw.trim();
    match property {
        "anchor-name" => name_list(raw, &["none"]),
        "anchor-scope" => name_list(raw, &["none", "all"]),
        "position-anchor" => {
            let lower = raw.to_ascii_lowercase();
            if matches!(lower.as_str(), "normal" | "none" | "auto" | "match-parent") {
                Some(lower)
            } else {
                dashed_ident(raw)
            }
        }
        "position-area" => {
            if raw.eq_ignore_ascii_case("none") {
                Some(String::from("none"))
            } else {
                position_area(raw, computed)
            }
        }
        "position-try-fallbacks" => position_try_fallbacks(raw),
        "position-try-order" => {
            let lower = raw.to_ascii_lowercase();
            matches!(lower.as_str(), "normal" | "most-width" | "most-height" | "most-block-size" | "most-inline-size").then_some(lower)
        }
        "position-visibility" => position_visibility(raw),
        _ => None,
    }
}

/// The specified value in canonical serialization order.
pub(crate) fn specified(property: &str, raw: &str) -> Option<String> {
    specified_or_computed(property, raw, false)
}

/// The computed value; only `position-area` reduces logical keywords.
pub(crate) fn computed(property: &str, raw: &str) -> Option<String> {
    specified_or_computed(property, raw, true)
}

/// `position-try` as its order and fallbacks longhands, both specified.
pub(crate) fn position_try(raw: &str, computed: bool) -> Option<(String, String)> {
    let raw = raw.trim();
    let first_end = raw.find(|character: char| character.is_ascii_whitespace()).unwrap_or(raw.len());
    let (head, rest) = raw.split_at(first_end);
    if let Some(order) = specified_or_computed("position-try-order", head, computed) {
        if !rest.trim().is_empty() {
            return Some((order, position_try_fallbacks(rest)?));
        }
    }
    Some((String::from("normal"), position_try_fallbacks(raw)?))
}

/// The shortest `position-try` serialization for its longhands.
pub(crate) fn serialize_position_try(order: &str, fallbacks: &str) -> String {
    if order == "normal" {
        fallbacks.to_owned()
    } else {
        alloc::format!("{order} {fallbacks}")
    }
}

pub(super) fn position_try_declarations(raw: &str, important: bool, out: &mut Vec<Declaration>) {
    for (name, value) in position_try_longhands(raw).into_iter().flatten() {
        if let Some(values) = extra_props::declarations(name, &value) {
            out.extend(values.into_iter().map(|value| Declaration { value, important }));
        }
    }
}

pub(super) fn position_try_longhands(raw: &str) -> Option<[(&'static str, String); 2]> {
    let (order, fallbacks) = position_try(raw, false)?;
    Some([("position-try-order", order), ("position-try-fallbacks", fallbacks)])
}

pub(super) fn computed_position_try(style: &Style) -> Option<String> {
    let order = extra_props::computed(style, "position-try-order")?;
    let fallbacks = extra_props::computed(style, "position-try-fallbacks")?;
    Some(serialize_position_try(&order, &fallbacks))
}

/// Track ranges (0 = before the anchor, 1 = anchor, 2 = after) a
/// `position-area` selects horizontally and vertically.
pub struct AreaTracks {
    pub horizontal: (u8, u8),
    pub vertical: (u8, u8),
}

fn track_of_side(side: usize, span: bool) -> (u8, u8) {
    let track = if side == 0 || side == 3 { 0 } else { 2 };
    match (track, span) {
        (0, false) => (0, 0),
        (0, true) => (0, 1),
        (_, false) => (2, 2),
        (_, true) => (1, 2),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    Block,
    Inline,
}

/// The physical side a non-center keyword names, with its axis.
fn keyword_side(base: &str, slot: Option<Slot>, containing: &[usize; 4], own: &[usize; 4]) -> Option<usize> {
    let (sides, name) = match base.strip_prefix("self-") {
        Some(rest) => (own, rest),
        None => (containing, base),
    };
    let logical = |slot: Slot, end: bool| sides[(if slot == Slot::Inline { 0 } else { 2 }) + usize::from(end)];
    Some(match name {
        "left" => 3,
        "right" => 1,
        "top" => 0,
        "bottom" => 2,
        "x-start" => sides_in_axis(sides, true).0,
        "x-end" => sides_in_axis(sides, true).1,
        "y-start" => sides_in_axis(sides, false).0,
        "y-end" => sides_in_axis(sides, false).1,
        "block-start" => logical(Slot::Block, false),
        "block-end" => logical(Slot::Block, true),
        "inline-start" => logical(Slot::Inline, false),
        "inline-end" => logical(Slot::Inline, true),
        "start" => logical(slot?, false),
        "end" => logical(slot?, true),
        _ => return None,
    })
}

fn keyword_selection(token: &str, slot: Option<Slot>, containing: &[usize; 4], own: &[usize; 4]) -> Option<(Option<bool>, (u8, u8))> {
    match token {
        "center" => return Some((None, (1, 1))),
        "span-all" => return Some((None, (0, 2))),
        _ => {}
    }
    let (span, base) = match token.strip_prefix("span-") {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let base = base.replacen("x-self-", "self-x-", 1).replacen("y-self-", "self-y-", 1);
    let side = keyword_side(&base, slot, containing, own)?;
    let explicit_axis = !matches!(base.trim_start_matches("self-"), "start" | "end");
    Some((explicit_axis.then(|| horizontal_side(side)), track_of_side(side, span)))
}

/// Resolves a computed `position-area` value to anchor-relative tracks.
pub fn area_tracks(value: &str, own: &[usize; 4], containing: &[usize; 4]) -> Option<AreaTracks> {
    let tokens = area_tokens(value)?;
    let side_axis = |slot: Slot, own_mode: bool| {
        let sides = if own_mode { own } else { containing };
        horizontal_side(sides[if slot == Slot::Inline { 0 } else { 2 }])
    };
    let place = |selections: [(Option<bool>, (u8, u8)); 2]| {
        let [first, second] = selections;
        match (first.0, second.0) {
            (Some(a), Some(b)) if a == b => None,
            (Some(horizontal), _) | (_, Some(horizontal)) => {
                let (a, b) = if first.0.is_some() { (first.1, second.1) } else { (second.1, first.1) };
                Some(if horizontal { AreaTracks { horizontal: a, vertical: b } } else { AreaTracks { horizontal: b, vertical: a } })
            }
            (None, None) => Some(AreaTracks { horizontal: first.1, vertical: second.1 }),
        }
    };
    match tokens.as_slice() {
        [one] => {
            let ambiguous = matches!(classify(one), Some(Axis::Start | Axis::SelfStart));
            if ambiguous {
                let own_mode = one.contains("self-");
                let block = keyword_selection(one, Some(Slot::Block), containing, own)?;
                let inline = keyword_selection(one, Some(Slot::Inline), containing, own)?;
                let block_horizontal = side_axis(Slot::Block, own_mode);
                let (block, inline) = (block.1, inline.1);
                return Some(if block_horizontal {
                    AreaTracks { horizontal: block, vertical: inline }
                } else {
                    AreaTracks { horizontal: inline, vertical: block }
                });
            }
            let (axis, range) = keyword_selection(one, None, containing, own)?;
            Some(match axis {
                Some(true) => AreaTracks { horizontal: range, vertical: (0, 2) },
                Some(false) => AreaTracks { horizontal: (0, 2), vertical: range },
                None => AreaTracks { horizontal: range, vertical: range },
            })
        }
        [a, b] => {
            let ambiguous = |token: &String| matches!(classify(token), Some(Axis::Start | Axis::SelfStart));
            let slots = if ambiguous(a) || ambiguous(b) {
                [Some(Slot::Block), Some(Slot::Inline)]
            } else {
                [None, None]
            };
            let first = keyword_selection(a, slots[0], containing, own)?;
            let second = keyword_selection(b, slots[1], containing, own)?;
            if slots[0].is_some() {
                let own_mode = a.contains("self-") || b.contains("self-");
                let block_horizontal = side_axis(Slot::Block, own_mode);
                let (block, inline) = (first.1, second.1);
                return Some(if block_horizontal {
                    AreaTracks { horizontal: block, vertical: inline }
                } else {
                    AreaTracks { horizontal: inline, vertical: block }
                });
            }
            place([first, second])
        }
        _ => None,
    }
}

/// The physical side an `auto` self-alignment faces within the tracks: the
/// side towards the anchor, centered, or stretched across all three tracks.
pub fn area_alignment(range: (u8, u8)) -> AreaAlignment {
    match range {
        (1, 1) => AreaAlignment::Center,
        (0, 2) => AreaAlignment::Stretch,
        (0, _) => AreaAlignment::TowardsEnd,
        _ => AreaAlignment::TowardsStart,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AreaAlignment {
    Center,
    Stretch,
    TowardsStart,
    TowardsEnd,
}

/// A `@position-try` rule: its name and the allowed declarations.
#[derive(Clone, Debug, PartialEq)]
pub struct PositionTryRule {
    pub name: Arc<str>,
    pub declarations: Arc<str>,
}

fn collect_position_try_nodes(input: &str, nodes: &[nesting::SourceRule], out: &mut Vec<PositionTryRule>) {
    for node in nodes {
        if node.kind == nesting::SourceRuleKind::PositionTry {
            let Some(name) = position_try_name(&input[node.prelude.clone()]) else {
                continue;
            };
            let mut block = DeclarationBlock::parse(&node.declaration_text(input)).unwrap_or_default();
            block.retain_properties(position_try_property);
            let declarations = block.serialize().unwrap_or_default();
            out.push(PositionTryRule { name: Arc::from(name), declarations: Arc::from(declarations) });
        }
        collect_position_try_nodes(input, &node.children, out);
    }
}

/// The `@position-try` rules of a stylesheet, in source order.
pub fn collect_position_try_rules(input: &str) -> Vec<PositionTryRule> {
    let mut rules = Vec::new();
    if !input.as_bytes().windows(13).any(|window| window.eq_ignore_ascii_case(b"@position-try")) {
        return rules;
    }
    if let Ok(nodes) = nesting::parse_source_rules(input, &[], false) {
        collect_position_try_nodes(input, &nodes, &mut rules);
    }
    rules
}

// ---------------------------------------------------------------------
// Position fallbacks
// ---------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tactic {
    Block,
    Inline,
    Start,
    X,
    Y,
}

/// One entry of a computed `position-try-fallbacks` list.
#[derive(Clone, Debug, PartialEq)]
pub struct Fallback {
    pub rule: Option<String>,
    pub tactics: Vec<Tactic>,
    pub area: Option<String>,
}

pub fn parse_fallbacks(computed: &str) -> Vec<Fallback> {
    let trimmed = computed.trim();
    if trimmed.eq_ignore_ascii_case("none") {
        return Vec::new();
    }
    let Some(items) = top_level_split(trimmed, b',', 33) else {
        return Vec::new();
    };
    items
        .into_iter()
        .map(|item| {
            let tokens = components(item).unwrap_or_default();
            let tactic_or_name = tokens.iter().all(|token| token.starts_with("--") || TACTICS.contains(&token.to_ascii_lowercase().as_str()));
            if !tactic_or_name {
                return Fallback { rule: None, tactics: Vec::new(), area: Some(item.trim().to_owned()) };
            }
            let mut fallback = Fallback { rule: None, tactics: Vec::new(), area: None };
            for token in tokens {
                match token.to_ascii_lowercase().as_str() {
                    "flip-block" => fallback.tactics.push(Tactic::Block),
                    "flip-inline" => fallback.tactics.push(Tactic::Inline),
                    "flip-start" => fallback.tactics.push(Tactic::Start),
                    "flip-x" => fallback.tactics.push(Tactic::X),
                    "flip-y" => fallback.tactics.push(Tactic::Y),
                    _ => fallback.rule = Some(token.to_owned()),
                }
            }
            fallback
        })
        .collect()
}

/// For each physical side, the side whose value it takes after the tactic.
pub fn tactic_permutation(tactic: Tactic, own: &[usize; 4]) -> [usize; 4] {
    let mut permutation = [0, 1, 2, 3];
    let mut swap = |a: usize, b: usize| permutation.swap(a, b);
    match tactic {
        Tactic::Block => swap(own[2], own[3]),
        Tactic::Inline => swap(own[0], own[1]),
        Tactic::X => swap(1, 3),
        Tactic::Y => swap(0, 2),
        Tactic::Start => {
            swap(own[2], own[0]);
            swap(own[3], own[1]);
        }
    }
    permutation
}

fn reflect(range: (u8, u8)) -> (u8, u8) {
    (2 - range.1, 2 - range.0)
}

pub fn permute_tracks(tracks: &AreaTracks, permutation: &[usize; 4]) -> AreaTracks {
    let horizontal = match permutation[3] {
        3 => tracks.horizontal,
        1 => reflect(tracks.horizontal),
        0 => tracks.vertical,
        _ => reflect(tracks.vertical),
    };
    let vertical = match permutation[0] {
        0 => tracks.vertical,
        2 => reflect(tracks.vertical),
        3 => tracks.horizontal,
        _ => reflect(tracks.horizontal),
    };
    AreaTracks { horizontal, vertical }
}

pub fn tracks_to_area(tracks: &AreaTracks) -> String {
    let name = |range: (u8, u8), low: &'static str, high: &'static str| match range {
        (0, 0) => low,
        (2, 2) => high,
        (1, 1) => "center",
        (0, 1) => if low == "left" { "span-left" } else { "span-top" },
        (1, 2) => if low == "left" { "span-right" } else { "span-bottom" },
        _ => "span-all",
    };
    let horizontal = name(tracks.horizontal, "left", "right");
    let vertical = name(tracks.vertical, "top", "bottom");
    if horizontal == "span-all" {
        vertical.to_owned()
    } else if vertical == "span-all" {
        horizontal.to_owned()
    } else {
        alloc::format!("{horizontal} {vertical}")
    }
}

fn side_keyword(side: usize) -> &'static str {
    ["top", "right", "bottom", "left"][side]
}

fn keyword_physical_side(keyword: &str) -> Option<usize> {
    ["top", "right", "bottom", "left"].iter().position(|name| *name == keyword)
}

/// Rewrites the `anchor()` side keywords of a value after the box taking
/// the inset of physical side `from` moved to side `to`.
pub fn flip_anchor_text(text: &str, permutation: &[usize; 4], from: usize, to: usize) -> String {
    let same_axis = horizontal_side(from) == horizontal_side(to);
    let flipped = from != to && same_axis;
    let mut output = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        if text.is_char_boundary(at) {
            if let Some(function) = function_at(text, at) {
                let open = at + function.len();
                if let Some(close) = closing_paren(text, open) {
                    let inner = &text[open + 1..close];
                    output.push_str(function);
                    output.push('(');
                    if function == "anchor" {
                        output.push_str(&flip_anchor_arguments(inner, permutation, flipped));
                    } else {
                        output.push_str(inner);
                    }
                    output.push(')');
                    at = close + 1;
                    continue;
                }
            }
        }
        let Some(ch) = text[at..].chars().next() else {
            break;
        };
        output.push(ch);
        at += ch.len_utf8();
    }
    output
}

fn flip_anchor_arguments(inner: &str, permutation: &[usize; 4], flipped: bool) -> String {
    let (main, fallback) = split_first_comma(inner);
    let tokens: Vec<&str> = main.split_ascii_whitespace().collect();
    let mut rewritten = Vec::with_capacity(tokens.len());
    for token in tokens {
        let lower = token.to_ascii_lowercase();
        let replacement = if let Some(side) = keyword_physical_side(&lower) {
            Some(side_keyword(permutation[side]).to_owned())
        } else if flipped {
            match lower.as_str() {
                "start" => Some(String::from("end")),
                "end" => Some(String::from("start")),
                "self-start" => Some(String::from("self-end")),
                "self-end" => Some(String::from("self-start")),
                other => other.strip_suffix('%').and_then(|number| number.parse::<f32>().ok()).map(|value| alloc::format!("{}%", 100.0 - value)),
            }
        } else {
            None
        };
        rewritten.push(replacement.unwrap_or_else(|| token.to_owned()));
    }
    let mut result = rewritten.join(" ");
    if let Some(fallback) = fallback {
        result.push(',');
        result.push_str(fallback);
    }
    result
}

/// Whether a longhand is an inset (false) or margin (true), and its physical side.
fn physical_side(longhand: &str, own: &[usize; 4]) -> Option<(bool, usize)> {
    let (margin, rest) = match longhand.strip_prefix("margin-") {
        Some(rest) => (true, rest),
        None => (false, longhand.strip_prefix("inset-").unwrap_or(longhand)),
    };
    let side = match rest {
        "top" => 0,
        "right" => 1,
        "bottom" => 2,
        "left" => 3,
        "inline-start" => own[0],
        "inline-end" => own[1],
        "block-start" => own[2],
        "block-end" => own[3],
        _ => return None,
    };
    Some((margin, side))
}

fn physical_name(margin: bool, side: usize) -> String {
    let name = side_keyword(side);
    if margin { alloc::format!("margin-{name}") } else { name.to_owned() }
}

fn swap_start_end(alignment: &mut Option<AlignItems>, keyword: &mut Option<AlignmentKeyword>) {
    let swapped = match alignment {
        Some(AlignItems::Start) => AlignItems::End,
        Some(AlignItems::End) => AlignItems::Start,
        _ => return,
    };
    *alignment = Some(swapped);
    *keyword = None;
}

/// Applies a position-try tactic's side permutation to insets, margins,
/// anchor functions and self-alignment.
pub(crate) fn flip_style(style: &mut Style, permutation: &[usize; 4]) {
    let own = style.logical_sides();
    let insets = [style.top, style.right, style.bottom, style.left];
    let margins = style.margin_sides;
    let autos = style.margin_auto;
    style.top = insets[permutation[0]];
    style.right = insets[permutation[1]];
    style.bottom = insets[permutation[2]];
    style.left = insets[permutation[3]];
    for side in 0..4 {
        style.margin_sides[side] = margins[permutation[side]];
        style.margin_auto[side] = autos[permutation[side]];
    }
    let remap = |slot: usize| match slot {
        35..=38 => 35 + permutation[slot - 35],
        39..=42 => 39 + permutation[slot - 39],
        other => other,
    };
    for (slot, _) in style.relative_lengths.iter_mut() {
        *slot = remap(*slot);
    }
    for expression in style.relative_expressions.iter_mut() {
        expression.slot = remap(expression.slot);
    }
    if let Some(entries) = style.anchored.clone() {
        let mut flipped: Vec<(usize, Arc<str>)> = Vec::with_capacity(entries.len());
        for (slot, entry) in entries.iter() {
            let Some((longhand, text)) = entry.split_once(':') else {
                flipped.push((*slot, entry.clone()));
                continue;
            };
            match physical_side(longhand, &own) {
                Some((margin, side)) => {
                    let target = permutation[side];
                    let name = physical_name(margin, target);
                    let Some(&new_slot) = slots(&name).first() else {
                        continue;
                    };
                    let rewritten = flip_anchor_text(text, permutation, side, target);
                    flipped.push((new_slot, Arc::from(alloc::format!("{name}:{rewritten}"))));
                }
                None => flipped.push((*slot, entry.clone())),
            }
        }
        style.anchored = Some(Arc::from(flipped));
    }
    if permutation[3] == 1 {
        let mut alignment = style.justify_self;
        let mut keyword = style.justify_self_keyword;
        swap_start_end(&mut alignment, &mut keyword);
        style.justify_self = alignment;
        style.justify_self_keyword = keyword;
    }
    if permutation[0] == 2 {
        let mut alignment = style.align_self;
        let mut keyword = style.align_self_keyword;
        swap_start_end(&mut alignment, &mut keyword);
        style.align_self = alignment;
        style.align_self_keyword = keyword;
    }
    if permutation[3] == 0 || permutation[3] == 2 {
        let justify = (style.justify_self, style.justify_self_keyword);
        style.justify_self = style.align_self;
        style.justify_self_keyword = style.align_self_keyword;
        style.align_self = justify.0;
        style.align_self_keyword = justify.1;
    }
}

/// Applies a declaration block, such as a `@position-try` body, to a style.
pub(crate) fn apply_block(style: &mut Style, text: &str) {
    let Ok(parsed) = declarations_with_variables(text, 0, false) else {
        return;
    };
    for declaration in parsed {
        let slot = declaration.value.slot();
        if let Some(entries) = style.anchored.clone() {
            if entries.iter().any(|(existing, _)| *existing == slot) && !matches!(declaration.value, Value::Anchor(..)) {
                let kept: Vec<(usize, Arc<str>)> = entries.iter().filter(|(existing, _)| *existing != slot).cloned().collect();
                style.anchored = if kept.is_empty() { None } else { Some(Arc::from(kept)) };
            }
        }
        match declaration.value {
            Value::Default(slot, _) => {
                let initial = Style::initial();
                super::copy_slot_state(slot, &initial, style, false);
            }
            Value::Revert(_) | Value::RevertLayer(_) | Value::Custom(..) | Value::Deferred(..) | Value::DeferredLonghand(..) | Value::DeferredSlot(..) => {}
            value => value.apply(style),
        }
    }
}

// ---------------------------------------------------------------------
// Resolution against anchor geometry
// ---------------------------------------------------------------------

/// What `anchor()` and `anchor-size()` resolve against for one declaration.
pub struct AnchorEnvironment<'a> {
    pub property: &'a str,
    /// Padding box of the containing block: x, y, width, height.
    pub containing: [f32; 4],
    /// Physical sides (0 top, 1 right, 2 bottom, 3 left) of inline-start,
    /// inline-end, block-start and block-end in the element's own writing mode.
    pub self_sides: [usize; 4],
    /// The same for the containing block's writing mode.
    pub containing_sides: [usize; 4],
    pub default_anchor: Option<&'a str>,
    /// Border box of the named anchor: x, y, width, height.
    pub lookup: &'a dyn Fn(&str) -> Option<[f32; 4]>,
}

fn horizontal_side(side: usize) -> bool {
    side == 1 || side == 3
}

fn opposite_side(side: usize) -> usize {
    (side + 2) % 4
}

/// The axis an inset, margin or size property lies on, and the physical side
/// for an inset property.
fn property_axis(env: &AnchorEnvironment<'_>) -> Option<(bool, Option<usize>)> {
    let own = &env.self_sides;
    Some(match env.property {
        "top" => (false, Some(0)),
        "right" => (true, Some(1)),
        "bottom" => (false, Some(2)),
        "left" => (true, Some(3)),
        "inset-inline-start" => (horizontal_side(own[0]), Some(own[0])),
        "inset-inline-end" => (horizontal_side(own[1]), Some(own[1])),
        "inset-block-start" => (horizontal_side(own[2]), Some(own[2])),
        "inset-block-end" => (horizontal_side(own[3]), Some(own[3])),
        "margin-top" | "height" | "min-height" | "max-height" => (false, None),
        "margin-right" | "margin-left" | "width" | "min-width" | "max-width" => (true, None),
        "margin-inline-start" | "margin-inline-end" | "inline-size" | "min-inline-size" | "max-inline-size" => {
            (horizontal_side(own[0]), None)
        }
        "margin-block-start" | "margin-block-end" | "block-size" | "min-block-size" | "max-block-size" => {
            (horizontal_side(own[2]), None)
        }
        "margin-bottom" => (false, None),
        _ => return None,
    })
}

fn sides_in_axis(sides: &[usize; 4], horizontal: bool) -> (usize, usize) {
    if horizontal_side(sides[0]) == horizontal {
        (sides[0], sides[1])
    } else {
        (sides[2], sides[3])
    }
}

fn edge_coordinate(anchor: &[f32; 4], side: usize) -> f32 {
    match side {
        0 => anchor[1],
        1 => anchor[0] + anchor[2],
        2 => anchor[1] + anchor[3],
        _ => anchor[0],
    }
}

fn split_first_comma(text: &str) -> (&str, Option<&str>) {
    match syntax::top_level_find(text, b',').flatten() {
        Some(at) => (&text[..at], Some(&text[at + 1..])),
        None => (text, None),
    }
}

fn closing_paren(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, ch) in text[open..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + at);
                }
            }
            _ => {}
        }
    }
    None
}

fn pixels(value: f32) -> String {
    alloc::format!("{}px", if value == 0.0 { 0.0 } else { value })
}

fn anchor_inset(inner: &str, env: &AnchorEnvironment<'_>) -> Option<f32> {
    let tokens: Vec<&str> = inner.split_ascii_whitespace().collect();
    let (name, side) = match tokens.as_slice() {
        [name, side] if name.starts_with("--") => (Some(*name), *side),
        [side] => (None, *side),
        _ => return None,
    };
    let (horizontal, property_side) = property_axis(env)?;
    let property_side = property_side?;
    let anchor = (env.lookup)(name.or(env.default_anchor)?)?;
    let physical = |keyword_horizontal: bool, side: usize| (keyword_horizontal == horizontal).then(|| edge_coordinate(&anchor, side));
    let (start, end) = sides_in_axis(&env.containing_sides, horizontal);
    let edge = match side.to_ascii_lowercase().as_str() {
        "top" => physical(false, 0)?,
        "bottom" => physical(false, 2)?,
        "left" => physical(true, 3)?,
        "right" => physical(true, 1)?,
        "inside" => edge_coordinate(&anchor, property_side),
        "outside" => edge_coordinate(&anchor, opposite_side(property_side)),
        "start" => edge_coordinate(&anchor, start),
        "end" => edge_coordinate(&anchor, end),
        "self-start" => edge_coordinate(&anchor, sides_in_axis(&env.self_sides, horizontal).0),
        "self-end" => edge_coordinate(&anchor, sides_in_axis(&env.self_sides, horizontal).1),
        "center" => {
            let (low, high) = if horizontal { (3, 1) } else { (0, 2) };
            (edge_coordinate(&anchor, low) + edge_coordinate(&anchor, high)) / 2.0
        }
        percentage => {
            let bare = percentage.strip_prefix("calc(").and_then(|rest| rest.strip_suffix(')')).unwrap_or(percentage);
            let fraction = bare.trim().strip_suffix('%')?.parse::<f32>().ok()? / 100.0;
            let from = edge_coordinate(&anchor, start);
            from + (edge_coordinate(&anchor, end) - from) * fraction
        }
    };
    let containing = env.containing;
    Some(match property_side {
        0 => edge - containing[1],
        1 => containing[0] + containing[2] - edge,
        2 => containing[1] + containing[3] - edge,
        _ => edge - containing[0],
    })
}

fn anchor_size(inner: &str, env: &AnchorEnvironment<'_>) -> Option<f32> {
    let tokens: Vec<&str> = inner.split_ascii_whitespace().collect();
    let (name, size) = match tokens.as_slice() {
        [] => (None, None),
        [name] if name.starts_with("--") => (Some(*name), None),
        [size] => (None, Some(*size)),
        [name, size] if name.starts_with("--") => (Some(*name), Some(*size)),
        _ => return None,
    };
    let (horizontal, _) = property_axis(env)?;
    let anchor = (env.lookup)(name.or(env.default_anchor)?)?;
    let width_axis = match size.map(str::to_ascii_lowercase).as_deref() {
        None => horizontal,
        Some("width") => true,
        Some("height") => false,
        Some("inline") => horizontal_side(env.containing_sides[0]),
        Some("block") => horizontal_side(env.containing_sides[2]),
        Some("self-inline") => horizontal_side(env.self_sides[0]),
        Some("self-block") => horizontal_side(env.self_sides[2]),
        Some(_) => return None,
    };
    Some(if width_axis { anchor[2] } else { anchor[3] })
}

fn resolve_function(function: &str, inner: &str, env: &AnchorEnvironment<'_>) -> Option<String> {
    let (main, fallback) = split_first_comma(inner);
    let resolved = if function == "anchor" { anchor_inset(main, env) } else { anchor_size(main, env) };
    match resolved {
        Some(value) => Some(pixels(value)),
        None => resolve(fallback?.trim(), env),
    }
}

/// Replaces every `anchor()` and `anchor-size()` in `text` by its length, or
/// `None` when one has neither a usable anchor nor a fallback.
pub fn resolve(text: &str, env: &AnchorEnvironment<'_>) -> Option<String> {
    let mut output = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        if text.is_char_boundary(at) {
            if let Some(function) = function_at(text, at) {
                let open = at + function.len();
                let close = closing_paren(text, open)?;
                output.push_str(&resolve_function(function, &text[open + 1..close], env)?);
                at = close + 1;
                continue;
            }
        }
        let ch = text[at..].chars().next()?;
        output.push(ch);
        at += ch.len_utf8();
    }
    Some(output)
}

/// Applies `longhand: value` to a computed style.
pub(crate) fn apply_resolved(style: &mut Style, longhand: &str, value: &str) {
    if let Ok(parsed) = declarations_with_variables(&alloc::format!("{longhand}:{value}"), 0, false) {
        for declaration in parsed {
            apply_value(style, declaration.value);
        }
    }
}

fn apply_value(style: &mut Style, value: Value) {
    let Value::ContextLength(slot, raw, nonnegative) = &value else {
        value.apply(style);
        return;
    };
    let context = style_length_context(None, style, MediaEnvironment::default(), Some(0.0));
    let Some(pixels) = contextual_length(raw, Some(context)) else {
        return;
    };
    style.relative_lengths.retain(|(existing, _)| existing != slot);
    if raw.contains('%') {
        let hundred_context = LengthContext { percent: Some(100.0), ..context };
        if let Some(hundred) = contextual_length(raw, Some(hundred_context)) {
            style.relative_lengths.push((*slot, RelativeLength { pixels, percent: hundred - pixels, nonnegative: *nonnegative }));
        }
    }
    let pixels = if *nonnegative { pixels.max(0.0) } else { pixels };
    if let Some(length) = length_value(*slot, pixels) {
        length.apply(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_anchor_functions_canonically() {
        assert_eq!(canonical_value("top", "anchor(top --foo, 1px)").as_deref(), Some("anchor(--foo top, 1px)"));
        assert_eq!(canonical_value("width", "anchor-size(width --foo)").as_deref(), Some("anchor-size(--foo width)"));
        assert_eq!(canonical_value("width", "anchor-size(--foo width, 0)").as_deref(), Some("anchor-size(--foo width, 0px)"));
        assert_eq!(canonical_value("top", "calc(anchor(--foo left, 1px) + 10%)").as_deref(), Some("calc(10% + anchor(--foo left, 1px))"));
        assert_eq!(
            canonical_value("top", "calc((anchor(--foo top) + anchor(--bar bottom)) / 2)").as_deref(),
            Some("calc(0.5 * (anchor(--foo top) + anchor(--bar bottom)))")
        );
        assert_eq!(canonical_value("width", "calc(anchor-size(--foo width, 0))").as_deref(), Some("anchor-size(--foo width, 0px)"));
        assert!(canonical_value("width", "anchor-size(--foo width, anchor(--bar top))").is_none());
        assert!(canonical_value("top", "anchor(--foo top,)").is_none());
        assert!(canonical_value("top", "anchor(foo top)").is_none());
    }

    #[test]
    fn position_area_forms() {
        assert_eq!(specified("position-area", "top left").as_deref(), Some("left top"));
        assert_eq!(specified("position-area", "center block-start").as_deref(), Some("block-start center"));
        assert_eq!(specified("position-area", "start span-all").as_deref(), Some("start span-all"));
        assert_eq!(computed("position-area", "block-start inline-end").as_deref(), Some("start end"));
        assert_eq!(computed("position-area", "start span-all").as_deref(), Some("block-start"));
        assert_eq!(computed("position-area", "span-all inline-start").as_deref(), Some("inline-start"));
        assert!(specified("position-area", "left left").is_none());
    }

    #[test]
    fn fallbacks_and_visibility() {
        assert_eq!(specified("position-try-fallbacks", "flip-inline --foo").as_deref(), Some("--foo flip-inline"));
        assert_eq!(specified("position-try-fallbacks", "top left, flip-x").as_deref(), Some("left top, flip-x"));
        assert!(specified("position-try-fallbacks", "--bar flip-block --foo").is_none());
        assert_eq!(specified("position-visibility", "no-overflow anchor-valid").as_deref(), Some("anchor-valid no-overflow"));
        assert!(specified("position-visibility", "always no-overflow").is_none());
    }
}
