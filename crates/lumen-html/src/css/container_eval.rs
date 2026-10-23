//! `@container` evaluation. A rule inside `@container` keeps one marker entry
//! per enclosing container rule in its media list; the cascade decides each
//! entry per element against the query containers a finished layout recorded
//! (`Oracle`). Style queries read the container's computed custom properties.
use super::container_query::{self, Condition, Kind, Piece, Token};
use super::parallel::SyncRefCell;
use super::*;
use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

const MARKER: &str = "@container ";
const MAX_ANCESTORS: usize = 512;
const MAX_EXPANDED: usize = 1 << 16;

pub(super) fn marker(prelude: &str) -> Option<Arc<str>> {
    let canonical = container_query::serialize_prelude(prelude)?;
    Some(Arc::from(alloc::format!("{MARKER}{canonical}")))
}

pub(super) fn is_marker(entry: &str) -> bool {
    entry.starts_with(MARKER)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Truth {
    True,
    False,
    /// Syntax the evaluator does not know: it poisons the whole condition.
    Unknown,
    /// A known feature the container cannot evaluate; combines like Kleene unknown.
    Unavailable,
}

impl Truth {
    fn known(value: bool) -> Self {
        if value { Self::True } else { Self::False }
    }

    fn not(self) -> Self {
        match self {
            Self::True => Self::False,
            Self::False => Self::True,
            Self::Unknown => Self::Unknown,
            Self::Unavailable => Self::Unavailable,
        }
    }
}

#[derive(Debug)]
enum Node {
    Not(Box<Node>),
    All(Vec<Node>),
    Any(Vec<Node>),
    Size(String),
    Style(String),
    Scroll(String),
    Anchored(String),
    Unknown,
}

/// The feature family a condition's leaf features belong to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Size,
    Style,
    Scroll,
    Anchored,
}

impl Node {
    /// Bit set of the size features the query reads, see `Feature::bit`.
    fn size_features(&self) -> u8 {
        match self {
            Self::Not(inner) => inner.size_features(),
            Self::All(nodes) | Self::Any(nodes) => nodes.iter().fold(0, |bits, node| bits | node.size_features()),
            Self::Size(content) => container_query::pieces(content).map_or(0, |parts| {
                parts.iter().fold(0, |bits, part| match part {
                    Piece::Word(word) => {
                        let base = word.strip_prefix("min-").or_else(|| word.strip_prefix("max-")).unwrap_or(word);
                        bits | feature(base).map_or(0, Feature::bit)
                    }
                    _ => bits,
                })
            }),
            Self::Style(_) | Self::Scroll(_) | Self::Anchored(_) | Self::Unknown => 0,
        }
    }

    fn uses(&self, mode: Mode) -> bool {
        match self {
            Self::Not(inner) => inner.uses(mode),
            Self::All(nodes) | Self::Any(nodes) => nodes.iter().any(|node| node.uses(mode)),
            Self::Size(_) => mode == Mode::Size,
            Self::Style(_) => mode == Mode::Style,
            Self::Scroll(_) => mode == Mode::Scroll,
            Self::Anchored(_) => mode == Mode::Anchored,
            Self::Unknown => false,
        }
    }
}

#[derive(Debug)]
pub(super) struct Parsed {
    name: Option<Arc<str>>,
    root: Option<Node>,
    size: bool,
    scroll: bool,
    anchored: bool,
    features: u8,
}

fn parse_conditions(entry: &str) -> Arc<[Parsed]> {
    let prelude = entry.get(MARKER.len()..).unwrap_or_default();
    container_query::parse_prelude(prelude).unwrap_or_default().iter().map(parse_condition).collect()
}

fn parse_condition(condition: &Condition) -> Parsed {
    let root = if condition.query.is_empty() {
        None
    } else {
        Some(container_query::tokenize(&condition.query).and_then(|tokens| parse_query(&tokens, Mode::Size)).unwrap_or(Node::Unknown))
    };
    let size = root.as_ref().is_some_and(|root| root.uses(Mode::Size));
    let scroll = root.as_ref().is_some_and(|root| root.uses(Mode::Scroll));
    let anchored = root.as_ref().is_some_and(|root| root.uses(Mode::Anchored));
    let features = root.as_ref().map_or(0, Node::size_features);
    Parsed { name: (!condition.name.is_empty()).then(|| Arc::from(condition.name.as_str())), root, size, scroll, anchored, features }
}

fn parse_query(tokens: &[Token<'_>], mode: Mode) -> Option<Node> {
    let first = tokens.first()?;
    if container_query::keyword(first, "not") {
        if tokens.len() != 2 {
            return Some(Node::Unknown);
        }
        return Some(Node::Not(Box::new(parse_group(tokens.get(1)?, mode)?)));
    }
    let head = parse_group(first, mode)?;
    if tokens.len() == 1 {
        return Some(head);
    }
    let all = container_query::keyword(&tokens[1], "and");
    if tokens.len() % 2 == 0 || !(all || container_query::keyword(&tokens[1], "or")) {
        return Some(Node::Unknown);
    }
    let mut nodes = alloc::vec![head];
    for pair in tokens[1..].chunks_exact(2) {
        if !container_query::keyword(&pair[0], if all { "and" } else { "or" }) {
            return Some(Node::Unknown);
        }
        nodes.push(parse_group(&pair[1], mode)?);
    }
    Some(if all { Node::All(nodes) } else { Node::Any(nodes) })
}

fn parse_group(token: &Token<'_>, mode: Mode) -> Option<Node> {
    match token.kind {
        Kind::Paren => Some(parse_content(&container_query::inner(token, 0), mode)),
        Kind::Function => {
            let open = token.text.find('(')?;
            let name = &token.text[..open];
            if mode == Mode::Size && name.eq_ignore_ascii_case("style") {
                Some(parse_content(&container_query::inner(token, open), Mode::Style))
            } else if mode == Mode::Size && name.eq_ignore_ascii_case("scroll-state") {
                Some(parse_content(&container_query::inner(token, open), Mode::Scroll))
            } else if mode == Mode::Size && name.eq_ignore_ascii_case("anchored") {
                Some(parse_content(&container_query::inner(token, open), Mode::Anchored))
            } else {
                Some(Node::Unknown)
            }
        }
        _ => None,
    }
}

fn parse_content(content: &str, mode: Mode) -> Node {
    if let Some(tokens) = container_query::tokenize(content) {
        let nested = tokens.first().is_some_and(|first| match first.kind {
            Kind::Paren => true,
            Kind::Function => ["style", "scroll-state", "anchored"].iter().any(|name| first.text.get(..name.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(name)) && first.text[name.len()..].starts_with('(')),
            Kind::Ident => container_query::keyword(first, "not") && tokens.get(1).is_some_and(|next| matches!(next.kind, Kind::Paren | Kind::Function)),
            Kind::Other => false,
        });
        if nested {
            return parse_query(&tokens, mode).unwrap_or(Node::Unknown);
        }
    }
    match mode {
        Mode::Style => Node::Style(content.into()),
        Mode::Scroll => Node::Scroll(content.into()),
        Mode::Anchored => Node::Anchored(content.into()),
        Mode::Size => Node::Size(content.into()),
    }
}

struct Subject<'a> {
    entry: Option<&'a Entry>,
    custom: &'a dyn CustomNames,
    length: &'a dyn Fn() -> LengthContext,
    /// Decides `name` and `name: value` style features when the caller knows more than the
    /// textual comparison, such as registered syntaxes; `None` is unknown.
    declared: Option<&'a dyn Fn(&str, Option<&str>) -> Option<bool>>,
}

/// Whether the style query `content` holds for `custom`; `None` when it is unknown.
pub(super) fn style_query_truth(
    content: &str,
    custom: &dyn CustomNames,
    length: &dyn Fn() -> LengthContext,
    declared: &dyn Fn(&str, Option<&str>) -> Option<bool>,
) -> Option<bool> {
    let node = parse_content(content, Mode::Style);
    let subject = Subject { entry: None, custom, length, declared: Some(declared) };
    match eval(&node, &subject) {
        Truth::True => Some(true),
        Truth::False => Some(false),
        Truth::Unknown | Truth::Unavailable => None,
    }
}

fn eval(node: &Node, subject: &Subject<'_>) -> Truth {
    match node {
        Node::Not(inner) => eval(inner, subject).not(),
        Node::All(nodes) => {
            let mut result = Truth::True;
            for node in nodes {
                match eval(node, subject) {
                    Truth::False => return Truth::False,
                    Truth::Unknown => result = Truth::Unknown,
                    Truth::Unavailable if result != Truth::Unknown => result = Truth::Unavailable,
                    Truth::Unavailable | Truth::True => {}
                }
            }
            result
        }
        Node::Any(nodes) => {
            let mut result = Truth::False;
            for node in nodes {
                match eval(node, subject) {
                    Truth::Unknown => return Truth::Unknown,
                    Truth::True => result = Truth::True,
                    Truth::Unavailable if result == Truth::False => result = Truth::Unavailable,
                    Truth::Unavailable | Truth::False => {}
                }
            }
            result
        }
        Node::Size(content) => subject.entry.map_or(Truth::Unknown, |entry| eval_size(content, entry)),
        Node::Style(content) => eval_style(content, subject),
        Node::Scroll(content) => subject.entry.map_or(Truth::Unknown, |entry| eval_scroll(content, entry)),
        Node::Anchored(content) => subject.entry.map_or(Truth::Unknown, |entry| eval_anchored(content, entry)),
        Node::Unknown => Truth::Unknown,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Feature {
    Width,
    Height,
    Inline,
    Block,
    Aspect,
    Orientation,
}

impl Feature {
    fn bit(self) -> u8 {
        1 << self as u8
    }
}

fn feature(name: &str) -> Option<Feature> {
    Some(match name {
        "width" => Feature::Width,
        "height" => Feature::Height,
        "inline-size" => Feature::Inline,
        "block-size" => Feature::Block,
        "aspect-ratio" => Feature::Aspect,
        "orientation" => Feature::Orientation,
        _ => return None,
    })
}

fn actual(feature: Feature, entry: &Entry) -> Option<f64> {
    let size = entry.kind == ContainerType::Size;
    let inline = entry.kind == ContainerType::InlineSize;
    let (width, height) = (entry.width.map(f64::from), entry.height.map(f64::from));
    match feature {
        Feature::Width => (size || (inline && !entry.vertical)).then_some(width).flatten(),
        Feature::Height => (size || (inline && entry.vertical)).then_some(height).flatten(),
        Feature::Inline => (size || inline).then_some(if entry.vertical { height } else { width }).flatten(),
        Feature::Block => size.then_some(if entry.vertical { width } else { height }).flatten(),
        Feature::Aspect => {
            let (width, height) = (width?, height?);
            size.then_some(width / height)
        }
        Feature::Orientation => {
            let (width, height) = (width?, height?);
            size.then_some(if height >= width { 1.0 } else { 0.0 })
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Sides {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Sides {
    const LEFT: Self = Self { left: true, right: false, top: false, bottom: false };
    const RIGHT: Self = Self { left: false, right: true, top: false, bottom: false };
    const TOP: Self = Self { left: false, right: false, top: true, bottom: false };
    const BOTTOM: Self = Self { left: false, right: false, top: false, bottom: true };

    fn union(self, other: Self) -> Self {
        Self {
            left: self.left || other.left,
            right: self.right || other.right,
            top: self.top || other.top,
            bottom: self.bottom || other.bottom,
        }
    }

    fn intersects(self, other: Self) -> bool {
        (self.left && other.left) || (self.right && other.right) || (self.top && other.top) || (self.bottom && other.bottom)
    }

    fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

fn inline_sides(entry: &Entry) -> (Sides, Sides) {
    match (entry.writing_mode, entry.rtl) {
        (WritingMode::HorizontalTb, false) => (Sides::LEFT, Sides::RIGHT),
        (WritingMode::HorizontalTb, true) => (Sides::RIGHT, Sides::LEFT),
        (WritingMode::SidewaysLr, false) => (Sides::BOTTOM, Sides::TOP),
        (WritingMode::SidewaysLr, true) => (Sides::TOP, Sides::BOTTOM),
        (_, false) => (Sides::TOP, Sides::BOTTOM),
        (_, true) => (Sides::BOTTOM, Sides::TOP),
    }
}

fn block_sides(entry: &Entry) -> (Sides, Sides) {
    match entry.writing_mode {
        WritingMode::HorizontalTb => (Sides::TOP, Sides::BOTTOM),
        WritingMode::VerticalRl | WritingMode::SidewaysRl => (Sides::RIGHT, Sides::LEFT),
        WritingMode::VerticalLr | WritingMode::SidewaysLr => (Sides::LEFT, Sides::RIGHT),
    }
}

/// The physical sides a `scroll-state()` direction keyword names.
fn direction_sides(entry: &Entry, keyword: &str) -> Option<Sides> {
    let (inline_start, inline_end) = inline_sides(entry);
    let (block_start, block_end) = block_sides(entry);
    Some(match keyword {
        "left" => Sides::LEFT,
        "right" => Sides::RIGHT,
        "top" => Sides::TOP,
        "bottom" => Sides::BOTTOM,
        "x" => Sides::LEFT.union(Sides::RIGHT),
        "y" => Sides::TOP.union(Sides::BOTTOM),
        "inline-start" => inline_start,
        "inline-end" => inline_end,
        "block-start" => block_start,
        "block-end" => block_end,
        "inline" => inline_start.union(inline_end),
        "block" => block_start.union(block_end),
        _ => return None,
    })
}

fn eval_anchored(content: &str, entry: &Entry) -> Truth {
    if !entry.anchored {
        return Truth::Unknown;
    }
    match anchored_query::feature(content) {
        Some(feature) => Truth::known(anchored_query::truth(&feature, entry.fallback.as_deref(), &entry.sides)),
        None => Truth::Unknown,
    }
}

fn eval_scroll(content: &str, entry: &Entry) -> Truth {
    if !entry.scroll_state {
        return Truth::Unknown;
    }
    let Some(content) = expand_vars(content, entry) else {
        return Truth::Unknown;
    };
    let Some(parts) = container_query::pieces(&content) else {
        return Truth::Unknown;
    };
    let (name, value) = match parts.as_slice() {
        [Piece::Word(name)] => (name.to_ascii_lowercase(), None),
        [Piece::Word(name), Piece::Colon, Piece::Word(value)] => (name.to_ascii_lowercase(), Some(value.to_ascii_lowercase())),
        _ => return Truth::Unknown,
    };
    match (name.as_str(), value.as_deref()) {
        ("scrollable", None) => Truth::known(entry.scrollable.any()),
        ("scrollable", Some("none")) => Truth::known(!entry.scrollable.any()),
        ("scrollable", Some(keyword)) => direction_sides(entry, keyword).map_or(Truth::Unknown, |sides| Truth::known(entry.scrollable.intersects(sides))),
        ("scrolled", None) | ("stuck", None) | ("snapped", None) => Truth::False,
        ("scrolled", Some("none")) | ("stuck", Some("none")) | ("snapped", Some("none")) => Truth::True,
        ("scrolled", Some(keyword)) => direction_sides(entry, keyword).map_or(Truth::Unknown, |_| Truth::False),
        ("stuck", Some(keyword)) if !matches!(keyword, "x" | "y" | "inline" | "block") => {
            direction_sides(entry, keyword).map_or(Truth::Unknown, |_| Truth::False)
        }
        ("snapped", Some("x" | "y" | "block" | "inline" | "both")) => Truth::False,
        _ => Truth::Unknown,
    }
}

fn quantize(value: f64) -> f64 {
    (value * 64.0).round() / 64.0
}

fn compare(left: f64, operator: &str, right: f64, lengths: bool) -> Option<bool> {
    let (left, right) = if lengths { (quantize(left), quantize(right)) } else { (left, right) };
    Some(match operator {
        "=" => left == right || (left - right).abs() <= left.abs().max(right.abs()) * 1e-9,
        "<" => left < right,
        "<=" => left <= right || (left - right).abs() <= left.abs().max(right.abs()) * 1e-9,
        ">" => left > right,
        ">=" => left >= right || (left - right).abs() <= left.abs().max(right.abs()) * 1e-9,
        _ => return None,
    })
}

fn length_value(raw: &str, context: LengthContext, units: ContainerUnitContext) -> Option<f64> {
    let expression = typed_numeric::parse_numeric_expression(raw)?;
    let pixels = typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Px);
    let zero = typed_numeric::parse_numeric_value(raw).is_some_and(|value| value.unit == typed_numeric::NumericUnit::Number && value.value == 0.0);
    if expression.numeric_type()? != pixels && !zero {
        return None;
    }
    let value = expression.evaluate(&mut FontAngleContext {
        length: Some(context),
        query: units,
        percent_scale: 0.01,
    })?;
    value.is_finite().then_some(value)
}

fn ratio_value(raw: &str) -> Option<f64> {
    let (numerator, denominator) = raw.split_once('/').unwrap_or((raw, "1"));
    let number = |text: &str| {
        let value = typed_numeric::parse_numeric_value(text.trim())?;
        (value.unit == typed_numeric::NumericUnit::Number && value.value >= 0.0 && value.value.is_finite()).then_some(value.value)
    };
    let (numerator, denominator) = (number(numerator)?, number(denominator)?);
    if numerator == 0.0 && denominator == 0.0 {
        return None;
    }
    Some(numerator / denominator)
}

fn expected(feature: Feature, raw: &str, entry: &Entry) -> Option<f64> {
    match feature {
        Feature::Aspect => ratio_value(raw),
        Feature::Orientation => None,
        _ => length_value(raw, entry.length, entry.units),
    }
}

/// `content` with its `var()` references substituted from the container; `None` when one is invalid.
fn expand_vars<'a>(content: &'a str, entry: &Entry) -> Option<alloc::borrow::Cow<'a, str>> {
    if !content.contains("var(") {
        return Some(alloc::borrow::Cow::Borrowed(content));
    }
    var_template::expand_variables_in(content, entry.custom.names(), MAX_EXPANDED).map(alloc::borrow::Cow::Owned)
}

fn eval_size(content: &str, entry: &Entry) -> Truth {
    let Some(content) = expand_vars(content, entry) else {
        return Truth::Unknown;
    };
    let content = &*content;
    let Some(parts) = container_query::pieces(content) else {
        return Truth::Unknown;
    };
    match parts.as_slice() {
        [Piece::Word(name)] => size_boolean(name, entry),
        [Piece::Word(name), Piece::Colon, rest @ ..] if !rest.is_empty() => size_plain(name, rest, entry),
        [Piece::Word(left), Piece::Operator(operator), Piece::Word(right)] => size_range(&[left, right], &[operator], entry),
        [Piece::Word(first), Piece::Operator(left), Piece::Word(middle), Piece::Operator(right), Piece::Word(last)] => {
            size_range(&[first, middle, last], &[left, right], entry)
        }
        _ => Truth::Unknown,
    }
}

fn size_boolean(name: &str, entry: &Entry) -> Truth {
    let Some(kind) = feature(name) else {
        return Truth::Unknown;
    };
    match actual(kind, entry) {
        None => Truth::Unavailable,
        Some(_) if kind == Feature::Orientation => Truth::True,
        Some(value) => Truth::known(value != 0.0),
    }
}

fn size_plain(name: &str, rest: &[Piece], entry: &Entry) -> Truth {
    let (base, operator) = if let Some(base) = name.strip_prefix("min-") {
        (base, ">=")
    } else if let Some(base) = name.strip_prefix("max-") {
        (base, "<=")
    } else {
        (name, "=")
    };
    let Some(kind) = feature(base) else {
        return Truth::Unknown;
    };
    let mut value = String::new();
    for piece in rest {
        let Piece::Word(word) = piece else {
            return Truth::Unknown;
        };
        if !value.is_empty() {
            value.push(' ');
        }
        value.push_str(word);
    }
    if kind == Feature::Orientation {
        if operator != "=" {
            return Truth::Unknown;
        }
        let portrait = if value.eq_ignore_ascii_case("portrait") {
            true
        } else if value.eq_ignore_ascii_case("landscape") {
            false
        } else {
            return Truth::Unknown;
        };
        return actual(kind, entry).map_or(Truth::Unavailable, |current| Truth::known((current == 1.0) == portrait));
    }
    let Some(current) = actual(kind, entry) else {
        return Truth::Unavailable;
    };
    let Some(wanted) = expected(kind, &value, entry) else {
        return Truth::Unknown;
    };
    compare(current, operator, wanted, kind != Feature::Aspect).map_or(Truth::Unknown, Truth::known)
}

fn flip(operator: &str) -> &str {
    match operator {
        "<" => ">",
        "<=" => ">=",
        ">" => "<",
        ">=" => "<=",
        other => other,
    }
}

fn size_range(operands: &[&String], operators: &[&&str], entry: &Entry) -> Truth {
    if operators.len() == 2 {
        let ascending = |operator: &str| matches!(operator, "<" | "<=");
        let descending = |operator: &str| matches!(operator, ">" | ">=");
        let consistent = (ascending(operators[0]) && ascending(operators[1])) || (descending(operators[0]) && descending(operators[1]));
        if !consistent {
            return Truth::Unknown;
        }
    }
    let name_at = operands.iter().position(|operand| feature(operand).is_some());
    let (Some(at), true) = (name_at, operators.len() + 1 == operands.len()) else {
        return Truth::Unknown;
    };
    if (operators.len() == 2 && at != 1) || (operators.len() == 1 && operands.iter().filter(|operand| feature(operand).is_some()).count() != 1) {
        return Truth::Unknown;
    }
    let Some(kind) = feature(operands[at]) else {
        return Truth::Unknown;
    };
    if kind == Feature::Orientation {
        return Truth::Unknown;
    }
    let Some(current) = actual(kind, entry) else {
        return Truth::Unavailable;
    };
    let lengths = kind != Feature::Aspect;
    let mut result = true;
    for (index, operator) in operators.iter().enumerate() {
        let (value_at, normalized) = if index < at { (index, **operator) } else { (index + 1, flip(operator)) };
        let Some(wanted) = expected(kind, operands[value_at], entry) else {
            return Truth::Unknown;
        };
        let holds = if index < at {
            compare(wanted, normalized, current, lengths)
        } else {
            compare(current, flip(normalized), wanted, lengths)
        };
        let Some(holds) = holds else {
            return Truth::Unknown;
        };
        result &= holds;
    }
    Truth::known(result)
}

struct Operators {
    colon: Option<usize>,
    spans: Vec<(usize, usize)>,
}

fn scan_top_level(text: &str) -> Operators {
    let bytes = text.as_bytes();
    let mut found = Operators { colon: None, spans: Vec::new() };
    let mut depth = 0usize;
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b'"' | b'\'' => {
                let quote = bytes[position];
                position += 1;
                while position < bytes.len() && bytes[position] != quote {
                    if bytes[position] == b'\\' {
                        position += 1;
                    }
                    position += 1;
                }
            }
            b'\\' => position += 1,
            b':' if depth == 0 && found.colon.is_none() => found.colon = Some(position),
            b'<' | b'>' | b'=' if depth == 0 => {
                let start = position;
                if bytes[position] != b'=' && bytes.get(position + 1) == Some(&b'=') {
                    position += 1;
                }
                found.spans.push((start, position + 1));
            }
            _ => {}
        }
        position += 1;
    }
    found
}

fn strip_important(value: &str) -> &str {
    let Some(bang) = value.rfind('!') else {
        return value;
    };
    if value[bang + 1..].trim().eq_ignore_ascii_case("important") {
        value[..bang].trim_end()
    } else {
        value
    }
}

fn custom_name(text: &str) -> bool {
    text.len() > 2 && text.starts_with("--") && !text.contains(|ch: char| ch.is_whitespace() || matches!(ch, '(' | ')' | ',' | '+' | '*' | '/' | '<' | '>' | '='))
}

fn eval_style(content: &str, subject: &Subject<'_>) -> Truth {
    let text = content.trim();
    let found = scan_top_level(text);
    let declaration = found.colon.filter(|colon| found.spans.first().is_none_or(|(start, _)| colon < start));
    if let Some(colon) = declaration {
        let name = text[..colon].trim();
        if !custom_name(name) {
            return Truth::Unknown;
        }
        let wanted = strip_important(text[colon + 1..].trim());
        if super::top_level_split(wanted, b';', usize::MAX).is_none_or(|parts| parts.len() > 1) {
            return Truth::Unknown;
        }
        let Some(wanted) = var_template::expand_variables_in(wanted, subject.custom, MAX_EXPANDED) else {
            return match subject.declared {
                Some(declared) => declared(name, None).map_or(Truth::Unknown, |present| Truth::known(!present)),
                None => Truth::known(subject.custom.value(name).is_none()),
            };
        };
        if let Some(declared) = subject.declared {
            return declared(name, Some(&wanted)).map_or(Truth::Unknown, Truth::known);
        }
        let Some(current) = subject.custom.value(name) else {
            return Truth::False;
        };
        return Truth::known(container_query::collapse_whitespace(current.as_str()) == container_query::collapse_whitespace(&wanted));
    }
    if found.spans.is_empty() {
        if !custom_name(text) {
            return Truth::Unknown;
        }
        if let Some(declared) = subject.declared {
            return declared(text, None).map_or(Truth::Unknown, Truth::known);
        }
        return Truth::known(subject.custom.value(text).is_some());
    }
    style_range(text, &found.spans, subject)
}

/// Whether `raw` is a single custom property, keyword, function or numeric term, so that a
/// failure to evaluate it makes the comparison false rather than unknown.
fn style_operand_syntax(raw: &str) -> bool {
    let raw = raw.trim();
    if raw.is_empty() {
        return false;
    }
    if custom_name(raw) || typed_numeric::parse_numeric_expression(raw).is_some() {
        return true;
    }
    container_query::tokenize(raw).is_some_and(|tokens| tokens.len() == 1 && matches!(tokens[0].kind, Kind::Ident | Kind::Function))
}

fn substitute_environment(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut index = 0;
    while index < bytes.len() {
        let boundary = index == 0 || !(bytes[index - 1].is_ascii_alphanumeric() || matches!(bytes[index - 1], b'-' | b'_'));
        if boundary && bytes[index..].len() >= 4 && bytes[index..index + 4].eq_ignore_ascii_case(b"env(") {
            if let Some(end) = safe_area_environment_end(bytes, index + 4) {
                output.push_str(&text[copied..index]);
                output.push_str("0px");
                copied = end;
                index = end;
                continue;
            }
        }
        index += 1;
    }
    output.push_str(&text[copied..]);
    output
}

/// End offset (after the closing parenthesis) of a `safe-area-inset-*` environment
/// variable reference whose name starts at `start`.
fn safe_area_environment_end(bytes: &[u8], start: usize) -> Option<usize> {
    let skip = |mut at: usize| {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        at
    };
    let name_start = skip(start);
    let mut name_end = name_start;
    while name_end < bytes.len() && (bytes[name_end].is_ascii_alphanumeric() || matches!(bytes[name_end], b'-' | b'_')) {
        name_end += 1;
    }
    let name = &bytes[name_start..name_end];
    let known = ["top", "right", "bottom", "left"].iter().any(|side| {
        name.len() == "safe-area-inset-".len() + side.len()
            && name[.."safe-area-inset-".len()].eq_ignore_ascii_case(b"safe-area-inset-")
            && name["safe-area-inset-".len()..].eq_ignore_ascii_case(side.as_bytes())
    });
    let close = skip(name_end);
    (known && bytes.get(close) == Some(&b')')).then_some(close + 1)
}

fn style_operand(raw: &str, subject: &Subject<'_>) -> Option<(f64, typed_numeric::NumericType)> {
    let raw = raw.trim();
    let text: String = if custom_name(raw) {
        subject.custom.value(raw)?.as_str().into()
    } else {
        var_template::expand_variables_in(raw, subject.custom, MAX_EXPANDED)?
    };
    let text = substitute_environment(&text);
    let expression = typed_numeric::parse_numeric_expression(text.trim())?;
    let kind = expression.numeric_type()?;
    let context = (subject.length)();
    let value = expression.evaluate(&mut FontAngleContext {
        length: Some(context),
        query: ContainerUnitContext::no_container(context.viewport),
        percent_scale: 1.0,
    })?;
    value.is_finite().then_some((value, kind))
}

fn style_range(text: &str, spans: &[(usize, usize)], subject: &Subject<'_>) -> Truth {
    if spans.len() > 2 {
        return Truth::Unknown;
    }
    let operators: Vec<&str> = spans.iter().map(|&(start, end)| &text[start..end]).collect();
    if operators.iter().any(|operator| !matches!(*operator, "<" | "<=" | ">" | ">=" | "=")) {
        return Truth::Unknown;
    }
    if operators.len() == 2 {
        let ascending = |operator: &str| matches!(operator, "<" | "<=");
        let descending = |operator: &str| matches!(operator, ">" | ">=");
        if !((ascending(operators[0]) && ascending(operators[1])) || (descending(operators[0]) && descending(operators[1]))) {
            return Truth::Unknown;
        }
    }
    let mut bounds = alloc::vec![0];
    for &(start, end) in spans {
        bounds.push(start);
        bounds.push(end);
    }
    bounds.push(text.len());
    let mut operands = Vec::new();
    let mut unevaluable = false;
    for pair in bounds.chunks_exact(2) {
        let raw = &text[pair[0]..pair[1]];
        if !style_operand_syntax(raw) {
            return Truth::Unknown;
        }
        match style_operand(raw, subject) {
            Some(operand) => operands.push(operand),
            None => unevaluable = true,
        }
    }
    if unevaluable {
        return Truth::False;
    }
    let mut result = true;
    for (index, operator) in operators.iter().enumerate() {
        let ((left, left_type), (right, right_type)) = (operands[index], operands[index + 1]);
        let number = typed_numeric::NumericType::default();
        let length = typed_numeric::NumericType::from_unit(typed_numeric::NumericUnit::Px);
        let comparable = left_type == right_type
            || (left_type == number && left == 0.0 && right_type == length)
            || (right_type == number && right == 0.0 && left_type == length);
        if !comparable {
            return Truth::False;
        }
        let Some(holds) = compare(left, operator, right, false) else {
            return Truth::Unknown;
        };
        result &= holds;
    }
    Truth::known(result)
}

/// Custom properties of a recorded container, compared by content.
#[derive(Clone)]
struct CustomRef(Option<Arc<CustomProperties>>);

impl CustomRef {
    fn names(&self) -> &dyn CustomNames {
        match &self.0 {
            Some(custom) => &**custom,
            None => &NO_CUSTOM,
        }
    }
}

impl core::fmt::Debug for CustomRef {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("CustomRef")
    }
}

impl PartialEq for CustomRef {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || (a.len() == b.len() && a.iter().eq(b.iter())),
            _ => false,
        }
    }
}

/// A query container as the last layout measured it. Sizes are content-box
/// sizes along the axes its `container-type` contains.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    names: Vec<Arc<str>>,
    kind: ContainerType,
    vertical: bool,
    width: Option<f32>,
    height: Option<f32>,
    length: LengthContext,
    units: ContainerUnitContext,
    custom: CustomRef,
    parent_custom: CustomRef,
    color: SourceColor,
    scroll_state: bool,
    anchored: bool,
    fallback: Option<Arc<str>>,
    sides: [usize; 4],
    scrollable: Sides,
    writing_mode: WritingMode,
    rtl: bool,
}

impl Entry {
    fn accepts(&self, condition: &Parsed) -> bool {
        condition.name.as_ref().is_none_or(|name| self.names.iter().any(|candidate| candidate == name))
            && (!condition.size || self.supports_features(condition.features))
            && (!condition.scroll || self.scroll_state)
            && (!condition.anchored || self.anchored)
    }

    /// Whether the container contains every axis the size features in `features` read.
    fn supports_features(&self, features: u8) -> bool {
        match self.kind {
            ContainerType::Normal => false,
            ContainerType::Size => true,
            ContainerType::InlineSize => {
                let physical_inline = if self.vertical { Feature::Height } else { Feature::Width };
                features & !(Feature::Inline.bit() | physical_inline.bit()) == 0
            }
        }
    }
}

/// A scroll container's offset and signed scroll limits, in CSS pixels.
#[derive(Clone, Copy, Debug)]
pub struct ScrollGeometry {
    pub min_x: f32,
    pub max_x: f32,
    pub min_y: f32,
    pub max_y: f32,
    pub x: f32,
    pub y: f32,
}

impl ScrollGeometry {
    fn scrollable(self) -> Sides {
        const EPSILON: f32 = 0.5;
        Sides {
            left: self.x - self.min_x > EPSILON,
            right: self.max_x - self.x > EPSILON,
            top: self.y - self.min_y > EPSILON,
            bottom: self.max_y - self.y > EPSILON,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Oracle(BTreeMap<u128, Entry>);

impl Oracle {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Records every element that is a query container: one with a `container-type`
/// or a `container-name`. A container without a measured box has unknown sizes,
/// which still stops the search for an ancestor container.
pub fn build_oracle(
    document: &Document,
    style_of: &dyn Fn(NodeId) -> Option<Arc<Style>>,
    measured: &dyn Fn(NodeId) -> Option<ContainerUnitContext>,
    scroll: &dyn Fn(NodeId) -> Option<ScrollGeometry>,
    fallback: &dyn Fn(NodeId) -> Option<Arc<str>>,
    environment: MediaEnvironment,
    text: Option<&dyn TextShaper>,
) -> Oracle {
    let mut entries = BTreeMap::new();
    let mut stack = alloc::vec![document.root()];
    while let Some(node) = stack.pop() {
        let mut child = document.first_child(node).ok().flatten();
        while let Some(id) = child {
            stack.push(id);
            child = document.next_sibling(id).ok().flatten();
        }
        if let Ok(Some(root)) = document.shadow_root(node) {
            stack.push(root);
        }
        if !matches!(document.kind(node), Ok(NodeKind::Element { .. })) {
            continue;
        }
        let Some(style) = style_of(node) else {
            continue;
        };
        let names: Vec<Arc<str>> = match extra_props::keyword(&style, "container-name") {
            Some(value) if !value.eq_ignore_ascii_case("none") => value.split_ascii_whitespace().map(Arc::from).collect(),
            _ => Vec::new(),
        };
        let kind = style.container_type;
        let context = if kind == ContainerType::Normal || !style.can_hold_size_containment() { None } else { measured(node) };
        if kind == ContainerType::Normal && names.is_empty() && !style.container_flags.scroll_state && !style.container_flags.anchored {
            continue;
        }
        let vertical = style.writing_mode != WritingMode::HorizontalTb;
        let basis = |basis: Option<ContainerUnitBasis>| match basis {
            Some(ContainerUnitBasis::Size(value)) => Some(value),
            _ => None,
        };
        let (width, height) = match (kind, context) {
            (ContainerType::Size, Some(context)) => (basis(Some(context.width)), basis(Some(context.height))),
            (ContainerType::InlineSize, Some(context)) if vertical => (None, basis(Some(context.height))),
            (ContainerType::InlineSize, Some(context)) => (basis(Some(context.width)), None),
            _ => (None, None),
        };
        entries.insert(node.key(), Entry {
            names,
            kind,
            vertical,
            width,
            height,
            length: style_length_context(text, &style, environment, None),
            units: outer_units(document, node, measured, &style, environment),
            custom: CustomRef(style.custom.clone()),
            parent_custom: CustomRef(
                document.composed_parent(node).ok().flatten().and_then(style_of).and_then(|parent| parent.custom.clone()),
            ),
            color: style.current_color_source(),
            scroll_state: style.container_flags.scroll_state,
            anchored: style.container_flags.anchored,
            fallback: if style.container_flags.anchored { fallback(node) } else { None },
            sides: style.logical_sides(),
            scrollable: if style.container_flags.scroll_state { scroll(node).map_or_else(Sides::default, ScrollGeometry::scrollable) } else { Sides::default() },
            writing_mode: style.writing_mode,
            rtl: style.direction == Direction::Rtl,
        });
    }
    Oracle(entries)
}

impl Style {
    /// Inline boxes and internal table boxes cannot apply size containment, so
    /// their size queries stay unknown even though `container-type` computes.
    pub(crate) fn can_hold_size_containment(&self) -> bool {
        !matches!(
            self.display,
            Display::None
                | Display::Contents
                | Display::Inline
                | Display::Table
                | Display::InlineTable
                | Display::TableCell
                | Display::TableCaption
        )
    }
}

/// Container-relative units in a query prelude resolve against the container
/// surrounding the queried one, not the queried container itself.
fn outer_units(
    document: &Document,
    node: NodeId,
    measured: &dyn Fn(NodeId) -> Option<ContainerUnitContext>,
    style: &Style,
    environment: MediaEnvironment,
) -> ContainerUnitContext {
    let mut current = document.composed_parent(node).ok().flatten();
    for _ in 0..MAX_ANCESTORS {
        let Some(ancestor) = current else {
            break;
        };
        if let Some(context) = measured(ancestor) {
            return context.for_writing_mode(style.writing_mode);
        }
        current = document.composed_parent(ancestor).ok().flatten();
    }
    ContainerUnitContext::no_container(environment).for_writing_mode(style.writing_mode)
}

/// Per-index state: the parsed `@container` preludes and the latest oracle.
pub(super) struct Support {
    oracle: SyncRefCell<Arc<Oracle>>,
    parsed: SyncRefCell<BTreeMap<Arc<str>, Arc<[Parsed]>>>,
    scanned: AtomicUsize,
    present: AtomicBool,
    epoch: AtomicU64,
}

impl Default for Support {
    fn default() -> Self {
        Self {
            oracle: SyncRefCell::new(Arc::new(Oracle::default())),
            parsed: SyncRefCell::new(BTreeMap::new()),
            scanned: AtomicUsize::new(0),
            present: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
        }
    }
}

impl Support {
    fn conditions(&self, entry: &Arc<str>) -> Arc<[Parsed]> {
        if let Some(found) = self.parsed.borrow().get(entry) {
            return found.clone();
        }
        let parsed = parse_conditions(entry);
        self.parsed.borrow_mut().insert(entry.clone(), parsed.clone());
        parsed
    }
}

impl StyleIndex {
    /// Whether any rule sits inside an `@container` rule.
    pub fn has_container_rules(&self) -> bool {
        let seen = self.containers.scanned.load(Ordering::Relaxed);
        if seen < self.rules.len() {
            if self.rules[seen..].iter().any(|rule| rule.media.iter().any(|entry| is_marker(entry))) {
                self.containers.present.store(true, Ordering::Relaxed);
            }
            self.containers.scanned.store(self.rules.len(), Ordering::Relaxed);
        }
        self.containers.present.load(Ordering::Relaxed)
    }

    /// Whether `node` is a scroll-state query container in the installed oracle.
    pub fn is_scroll_state_container(&self, node: NodeId) -> bool {
        self.containers.oracle.borrow().0.get(&node.key()).is_some_and(|entry| entry.scroll_state)
    }

    /// Changes whenever `replace_container_oracle` installed different containers.
    pub fn container_epoch(&self) -> u64 {
        self.containers.epoch.load(Ordering::Relaxed)
    }

    /// Installs the containers a layout measured; `true` when they differ from the previous ones.
    pub fn replace_container_oracle(&self, oracle: Oracle) -> bool {
        if **self.containers.oracle.borrow() == oracle {
            return false;
        }
        *self.containers.oracle.borrow_mut() = Arc::new(oracle);
        self.containers.epoch.fetch_add(1, Ordering::Relaxed);
        true
    }

    pub(super) fn container_rule_matches(
        &self,
        order: usize,
        document: &Document,
        node: NodeId,
        pseudo: bool,
        parent: Option<&Style>,
        text: Option<&dyn TextShaper>,
    ) -> bool {
        let oracle = self.containers.oracle.borrow();
        let mut resolved: Vec<(Option<Arc<str>>, bool, Option<&Entry>)> = Vec::new();
        self.rules[order].media.iter().filter(|entry| is_marker(entry)).all(|entry| {
            self.containers.conditions(entry).iter().any(|condition| {
                self.condition_matches(condition, &oracle, &mut resolved, document, node, pseudo, parent, text)
            })
        })
    }

    /// Whether `prelude`, a `<container-condition>#` list, holds for `node` as if an
    /// `@container` rule had styled it. `parent` is the node's composed parent style.
    pub fn container_query_matches(
        &self,
        document: &Document,
        node: NodeId,
        prelude: &str,
        parent: Option<&Style>,
        text: Option<&dyn TextShaper>,
    ) -> bool {
        let Some(canonical) = container_query::serialize_prelude(prelude) else {
            return false;
        };
        let conditions = parse_conditions(&alloc::format!("{MARKER}{canonical}"));
        let oracle = self.containers.oracle.borrow();
        let mut resolved: Vec<(Option<Arc<str>>, bool, Option<&Entry>)> = Vec::new();
        conditions.iter().any(|condition| {
            self.condition_matches(condition, &oracle, &mut resolved, document, node, false, parent, text)
        })
    }

    fn condition_matches<'o>(
        &self,
        condition: &Parsed,
        oracle: &'o Oracle,
        resolved: &mut Vec<(Option<Arc<str>>, bool, Option<&'o Entry>)>,
        document: &Document,
        node: NodeId,
        pseudo: bool,
        parent: Option<&Style>,
        text: Option<&dyn TextShaper>,
    ) -> bool {
        if condition.name.is_none() && !condition.size {
            let (Some(parent), Some(root)) = (parent, condition.root.as_ref()) else {
                return false;
            };
            let environment = self.environment;
            let length = || style_length_context(text, parent, environment, None);
            let registrations = self.registered_snapshot();
            let context = (length(), ContainerUnitContext::no_container(environment), parent.current_color_source());
            let scope = if_function::IfScope {
                names: parent.custom_properties(),
                base: None,
                registrations: &registrations,
                context: &context,
                environment,
                current: None,
            };
            let declared = |name: &str, wanted: Option<&str>| scope.declared(name, wanted);
            let subject = Subject { entry: None, custom: parent.custom_properties(), length: &length, declared: Some(&declared) };
            return eval(root, &subject) == Truth::True;
        }
        if oracle.0.is_empty() {
            return false;
        }
        let cached = resolved.iter().find(|(name, size, _)| *name == condition.name && *size == condition.size);
        let found = match cached {
            Some((_, _, found)) => *found,
            None => {
                let found = Self::find_container(condition, oracle, document, node, pseudo);
                resolved.push((condition.name.clone(), condition.size, found));
                found
            }
        };
        let Some(entry) = found else {
            return false;
        };
        let Some(root) = condition.root.as_ref() else {
            return true;
        };
        let length = || entry.length;
        let registrations = self.registered_snapshot();
        let context = (entry.length, entry.units, entry.color.clone());
        let scope = if_function::IfScope {
            names: entry.custom.names(),
            base: Some(entry.parent_custom.names()),
            registrations: &registrations,
            context: &context,
            environment: self.environment,
            current: None,
        };
        let declared = |name: &str, wanted: Option<&str>| scope.declared(name, wanted);
        let subject = Subject { entry: Some(entry), custom: entry.custom.names(), length: &length, declared: Some(&declared) };
        eval(root, &subject) == Truth::True
    }

    fn find_container<'o>(
        condition: &Parsed,
        oracle: &'o Oracle,
        document: &Document,
        node: NodeId,
        pseudo: bool,
    ) -> Option<&'o Entry> {
        let mut current = if pseudo { Some(node) } else { document.composed_parent(node).ok().flatten() };
        let mut depth = 0;
        while let Some(id) = current {
            depth += 1;
            if depth > MAX_ANCESTORS {
                break;
            }
            if let Some(entry) = oracle.0.get(&id.key()).filter(|entry| entry.accepts(condition)) {
                return Some(entry);
            }
            current = document.composed_parent(id).ok().flatten();
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: ContainerType, vertical: bool, width: f32, height: f32) -> Entry {
        Entry {
            names: Vec::new(),
            kind,
            vertical,
            width: Some(width),
            height: Some(height),
            length: static_length_context(),
            units: ContainerUnitContext::no_container(static_length_context().viewport),
            custom: CustomRef(None),
            parent_custom: CustomRef(None),
            color: Style::initial().current_color_source(),
            scroll_state: false,
            anchored: false,
            fallback: None,
            sides: [0, 1, 2, 3],
            scrollable: Sides::default(),
            writing_mode: WritingMode::HorizontalTb,
            rtl: false,
        }
    }

    fn truth(query: &str, entry: &Entry) -> Truth {
        let tokens = container_query::tokenize(query).unwrap();
        let root = parse_query(&tokens, Mode::Size).unwrap();
        let length = || entry.length;
        eval(&root, &Subject { entry: Some(entry), custom: entry.custom.names(), length: &length, declared: None })
    }

    #[test]
    fn size_features_follow_container_axes() {
        let horizontal = entry(ContainerType::Size, false, 100.0, 200.0);
        let vertical = entry(ContainerType::Size, true, 100.0, 200.0);
        assert_eq!(truth("(width >= 100px)", &horizontal), Truth::True);
        assert_eq!(truth("(inline-size < 100px)", &horizontal), Truth::False);
        assert_eq!(truth("(inline-size: 200px)", &vertical), Truth::True);
        assert_eq!(truth("(orientation: portrait)", &horizontal), Truth::True);
        assert_eq!(truth("(aspect-ratio: 1/2)", &horizontal), Truth::True);
        assert_eq!(truth("(100px < width < 200px)", &horizontal), Truth::False);
        assert_eq!(truth("(100px <= width <= 200px)", &horizontal), Truth::True);
        assert_eq!(truth("(100px < width > 200px)", &horizontal), Truth::Unknown);
    }

    #[test]
    fn inline_size_containers_leave_the_block_axis_unknown() {
        let inline = entry(ContainerType::InlineSize, false, 100.0, 0.0);
        assert_eq!(truth("(width: 100px)", &inline), Truth::True);
        assert_eq!(truth("(height)", &inline), Truth::Unavailable);
        assert_eq!(truth("(not (height))", &inline), Truth::Unavailable);
    }

    #[test]
    fn scroll_state_features_follow_writing_mode() {
        let mut container = entry(ContainerType::Normal, false, 0.0, 0.0);
        container.scroll_state = true;
        container.scrollable = Sides::RIGHT;
        let scroll = |query: &str, entry: &Entry| {
            let tokens = container_query::tokenize(query).unwrap();
            let root = parse_query(&tokens, Mode::Size).unwrap();
            let length = || entry.length;
            eval(&root, &Subject { entry: Some(entry), custom: entry.custom.names(), length: &length, declared: None })
        };
        assert_eq!(scroll("scroll-state(scrollable: right)", &container), Truth::True);
        assert_eq!(scroll("scroll-state(scrollable: inline-end)", &container), Truth::True);
        assert_eq!(scroll("scroll-state(scrollable: left)", &container), Truth::False);
        assert_eq!(scroll("scroll-state(scrollable: auto)", &container), Truth::Unknown);
        container.rtl = true;
        assert_eq!(scroll("scroll-state(scrollable: inline-end)", &container), Truth::False);
        assert_eq!(scroll("scroll-state(scrolled: none)", &container), Truth::True);
    }

    #[test]
    fn unknown_operands_poison_or() {
        let size = entry(ContainerType::Size, false, 1.0, 0.0);
        assert_eq!(truth("((unknown) or (width))", &size), Truth::Unknown);
        assert_eq!(truth("((height) or (width))", &size), Truth::True);
        assert_eq!(truth("((unknown) and (height))", &size), Truth::False);
    }
}
