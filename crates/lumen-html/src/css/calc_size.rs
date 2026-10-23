//! `calc-size()` (CSS Values 5): an intrinsic-size keyword basis combined with
//! a calculation that may refer to the basis through `size`. Every supported
//! calculation is affine in `size`, so a computed value is a copyable
//! `px + frac * containing + k * size` triple resolved by layout.
use super::*;
use alloc::{boxed::Box, format, string::String, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// `calc-size(any, ...)`: no intrinsic basis; the value is a plain length
    /// that keeps its calc-size identity for interpolation.
    Any,
    Auto,
    MinContent,
    MaxContent,
    FitContent,
    Stretch,
    Content,
    /// `fit-content(<length-percentage>)`: the fit-content size with the
    /// available space replaced by the argument held in `px` and `frac`.
    FitContentArgument,
}

impl Basis {
    pub fn keyword(self) -> &'static str {
        match self {
            Self::Any => "any",
            Self::Auto => "auto",
            Self::MinContent => "min-content",
            Self::MaxContent => "max-content",
            Self::FitContent => "fit-content",
            Self::Stretch => "stretch",
            Self::Content => "content",
            Self::FitContentArgument => "fit-content",
        }
    }
    pub fn needs_minimum(self) -> bool {
        matches!(self, Self::Auto | Self::MinContent | Self::FitContent | Self::FitContentArgument)
    }
    pub fn needs_maximum(self) -> bool {
        matches!(self, Self::Auto | Self::MaxContent | Self::FitContent | Self::Content | Self::FitContentArgument)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CalcSizeValue {
    pub basis: Basis,
    pub px: f32,
    pub frac: f32,
    pub k: f32,
    /// Whether the source calculation wrote a `size` term (even with a zero
    /// coefficient) or a length term, so serialization keeps its shape.
    pub size_term: bool,
    pub px_term: bool,
    /// Number of `calc-size()` layers the source nested around the basis.
    pub nest: u8,
}

impl CalcSizeValue {
    /// `None` when a percentage has no definite containing size, in which
    /// case the property behaves as `auto`.
    pub fn resolve(self, size: f32, containing: Option<f32>) -> Option<f32> {
        if self.basis == Basis::FitContentArgument {
            return Some(size);
        }
        let percent = self.frac * containing.unwrap_or(0.0);
        Some((self.px + percent + self.k * size).max(0.0))
    }
    /// The `fit-content()` argument against its percentage basis. An
    /// indefinite basis leaves the argument unbounded.
    pub fn fit_content_limit(self, containing: Option<f32>) -> f32 {
        let percent = if self.frac == 0.0 { Some(0.0) } else { containing.map(|size| self.frac * size) };
        percent.map_or(f32::INFINITY, |percent| (self.px + percent).max(0.0))
    }
    pub fn serialize(self) -> String {
        if self.basis == Basis::FitContentArgument {
            let mut lin = Lin::length();
            if self.px != 0.0 || self.frac == 0.0 {
                lin.units.push((String::from("px"), f64::from(self.px)));
            }
            if self.frac != 0.0 {
                lin.pct = Some(f64::from(self.frac) * 100.0);
            }
            return format!("fit-content({})", lin.text(true));
        }
        let mut lin = Lin::length();
        if self.px_term || (self.frac == 0.0 && !self.size_term) {
            lin.units.push((String::from("px"), f64::from(self.px)));
        }
        if self.frac != 0.0 {
            lin.pct = Some(f64::from(self.frac) * 100.0);
        }
        lin.size = self.size_term;
        lin.k = f64::from(self.k);
        nested_text(self.basis.keyword(), &lin.text(false), self.nest)
    }
}

/// `calc-size(basis, expression)` with identity layers standing for the extra
/// nesting of the source, which bounds how far interpolation may expand it.
fn nested_text(basis: &str, expression: &str, nest: u8) -> String {
    let mut inner = String::from(basis);
    for _ in 1..nest {
        inner = format!("calc-size({inner}, size)");
    }
    format!("calc-size({inner}, {expression})")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Preferred,
    Min,
    Max,
    FlexBasis,
}

fn property(name: &str) -> Option<(Kind, usize)> {
    Some(match name {
        "width" => (Kind::Preferred, 3),
        "height" => (Kind::Preferred, 4),
        "min-width" => (Kind::Min, 23),
        "max-width" => (Kind::Max, 24),
        "min-height" => (Kind::Min, 51),
        "max-height" => (Kind::Max, 52),
        "inline-size" => (Kind::Preferred, 87),
        "block-size" => (Kind::Preferred, 88),
        "min-inline-size" => (Kind::Min, 91),
        "max-inline-size" => (Kind::Max, 92),
        "min-block-size" => (Kind::Min, 93),
        "max-block-size" => (Kind::Max, 94),
        "flex-basis" => (Kind::FlexBasis, 20),
        _ => return None,
    })
}

fn kind_of_slot(slot: usize) -> Option<Kind> {
    Some(match slot {
        3 | 4 | 87 | 88 => Kind::Preferred,
        23 | 51 | 91 | 93 => Kind::Min,
        24 | 52 | 92 | 94 => Kind::Max,
        20 => Kind::FlexBasis,
        _ => return None,
    })
}

const MAX_INPUT: usize = 1024;
const MAX_DEPTH: usize = 40;

/// Deeper `calc-size()` nesting would expand too far to interpolate.
const MAX_INTERPOLATED_NEST: u8 = 16;

const UNITS: &[&str] = &[
    "px", "em", "rem", "ex", "rex", "ch", "rch", "cap", "rcap", "ic", "ric", "lh", "rlh", "vw", "vh",
    "vi", "vb", "vmin", "vmax", "svw", "svh", "svi", "svb", "svmin", "svmax", "lvw", "lvh", "lvi",
    "lvb", "lvmin", "lvmax", "dvw", "dvh", "dvi", "dvb", "dvmin", "dvmax", "cqw", "cqh", "cqi",
    "cqb", "cqmin", "cqmax", "in", "cm", "mm", "q", "pt", "pc",
];

fn absolute_factor(unit: &str) -> Option<f64> {
    Some(match unit {
        "in" => 96.0,
        "cm" => 96.0 / 2.54,
        "mm" => 96.0 / 25.4,
        "q" => 96.0 / 101.6,
        "pt" => 96.0 / 72.0,
        "pc" => 16.0,
        _ => return None,
    })
}

fn number_text(value: f64) -> String {
    crate::animation::serialize_numeric(value, "").unwrap_or_default()
}

/// A linear combination of absolute/relative lengths, one percentage and
/// `size`. Pure numbers are tracked separately so `2 * size` stays typed.
#[derive(Clone, Debug, PartialEq)]
struct Lin {
    dim: bool,
    num: f64,
    pct: Option<f64>,
    units: Vec<(String, f64)>,
    k: f64,
    size: bool,
}

impl Lin {
    fn number(value: f64) -> Self {
        Self { dim: false, num: value, pct: None, units: Vec::new(), k: 0.0, size: false }
    }
    fn length() -> Self {
        Self { dim: true, ..Self::number(0.0) }
    }
    fn size() -> Self {
        Self { size: true, k: 1.0, ..Self::length() }
    }
    fn scaled(&self, factor: f64) -> Self {
        Self {
            dim: self.dim,
            num: self.num * factor,
            pct: self.pct.map(|value| value * factor),
            units: self.units.iter().map(|(unit, value)| (unit.clone(), value * factor)).collect(),
            k: self.k * factor,
            size: self.size,
        }
    }
    fn merge(&mut self, other: &Self) {
        self.num += other.num;
        if let Some(value) = other.pct {
            self.pct = Some(self.pct.unwrap_or(0.0) + value);
        }
        for (unit, value) in &other.units {
            match self.units.iter_mut().find(|(existing, _)| existing == unit) {
                Some(entry) => entry.1 += value,
                None => self.units.push((unit.clone(), *value)),
            }
        }
        self.k += other.k;
        self.size |= other.size;
    }
    fn add(mut self, other: &Self) -> Option<Self> {
        if self.dim != other.dim {
            return None;
        }
        self.merge(other);
        Some(self)
    }
    fn multiply(self, other: Self) -> Option<Self> {
        match (self.dim, other.dim) {
            (true, true) => None,
            (false, false) => Some(Self::number(self.num * other.num)),
            (false, true) => Some(other.scaled(self.num)),
            (true, false) => Some(self.scaled(other.num)),
        }
    }
    fn divide(self, other: Self) -> Option<Self> {
        if other.dim || other.num == 0.0 || !other.num.is_finite() {
            return None;
        }
        Some(self.scaled(1.0 / other.num))
    }
    fn substitute(&self, replacement: &Self) -> Self {
        let mut out = self.clone();
        out.k = 0.0;
        out.size = false;
        if self.size {
            out.merge(&replacement.scaled(self.k));
            out.size = replacement.size;
        }
        out
    }
    fn finite(&self) -> bool {
        self.num.is_finite()
            && self.k.is_finite()
            && self.pct.is_none_or(f64::is_finite)
            && self.units.iter().all(|(_, value)| value.is_finite())
    }

    fn piece_count(&self) -> usize {
        usize::from(self.pct.is_some()) + self.units.len() + usize::from(self.size)
    }

    fn pieces(&self, parenthesize_size: bool) -> Vec<(bool, String)> {
        let mut pieces: Vec<(bool, String)> = Vec::new();
        let mut push = |value: f64, tail: &str| {
            pieces.push((value.is_sign_negative() && value != 0.0, format!("{}{tail}", number_text(value.abs()))));
        };
        if let Some(value) = self.pct {
            push(value, "%");
        }
        let mut ordered: Vec<&(String, f64)> = self.units.iter().collect();
        ordered.sort_by_key(|(unit, _)| unit != "px");
        for (unit, value) in ordered {
            push(*value, unit);
        }
        if self.size {
            let body = if self.k == 1.0 {
                String::from("size")
            } else if parenthesize_size {
                format!("({} * size)", number_text(self.k))
            } else {
                format!("{} * size", number_text(self.k))
            };
            pieces.push((false, body));
        }
        pieces
    }

    /// Source-order independent canonical text; `calc_wrap` wraps sums that
    /// are not a lone term in `calc()`.
    fn text(&self, calc_wrap: bool) -> String {
        let pieces = self.pieces(self.piece_count() > 1);
        if pieces.is_empty() {
            return String::from("0px");
        }
        let mut out = String::new();
        for (index, (negative, body)) in pieces.iter().enumerate() {
            if index == 0 {
                if *negative {
                    out.push('-');
                }
            } else if *negative {
                out.push_str(" - ");
            } else {
                out.push_str(" + ");
            }
            out.push_str(body);
        }
        if calc_wrap && pieces.len() > 1 {
            format!("calc({out})")
        } else {
            out
        }
    }
    fn calc_text(&self) -> String {
        let text = self.text(false);
        format!("calc({text})")
    }
}

struct Parser<'a> {
    text: &'a str,
    index: usize,
    depth: usize,
}

impl<'a> Parser<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.index..]
    }
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.index).copied()
    }
    fn skip_space(&mut self) -> bool {
        let start = self.index;
        while self.peek().is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0c)) {
            self.index += 1;
        }
        self.index > start
    }
    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn identifier(&mut self) -> &'a str {
        let start = self.index;
        while self.peek().is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_') {
            self.index += 1;
        }
        &self.text[start..self.index]
    }
    fn at_function(&self, name: &str) -> bool {
        let rest = self.rest().as_bytes();
        rest.len() > name.len()
            && rest[..name.len()].eq_ignore_ascii_case(name.as_bytes())
            && rest[name.len()] == b'('
    }

    fn sum(&mut self, allow_size: bool) -> Option<Lin> {
        let mut total = self.product(allow_size)?;
        loop {
            let save = self.index;
            let spaced = self.skip_space();
            let negative = match self.peek() {
                Some(b'+') if spaced => false,
                Some(b'-') if spaced => true,
                _ => {
                    self.index = save;
                    return Some(total);
                }
            };
            self.index += 1;
            if !self.skip_space() {
                return None;
            }
            let term = self.product(allow_size)?;
            total = total.add(&if negative { term.scaled(-1.0) } else { term })?;
        }
    }

    fn product(&mut self, allow_size: bool) -> Option<Lin> {
        let mut total = self.unary(allow_size)?;
        loop {
            let save = self.index;
            self.skip_space();
            let divide = match self.peek() {
                Some(b'*') => false,
                Some(b'/') => true,
                _ => {
                    self.index = save;
                    return Some(total);
                }
            };
            self.index += 1;
            self.skip_space();
            let factor = self.unary(allow_size)?;
            total = if divide { total.divide(factor)? } else { total.multiply(factor)? };
        }
    }

    fn group(&mut self, allow_size: bool) -> Option<Lin> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return None;
        }
        self.skip_space();
        let inner = self.sum(allow_size)?;
        self.skip_space();
        if !self.eat(b')') {
            return None;
        }
        self.depth -= 1;
        Some(inner)
    }

    fn unary(&mut self, allow_size: bool) -> Option<Lin> {
        self.skip_space();
        match self.peek()? {
            b'(' => {
                self.index += 1;
                self.group(allow_size)
            }
            byte if byte.is_ascii_digit() || matches!(byte, b'.' | b'+' | b'-') => self.numeric(),
            byte if byte.is_ascii_alphabetic() => {
                if self.at_function("calc") {
                    self.index += "calc(".len();
                    return self.group(allow_size);
                }
                let name = self.identifier();
                if allow_size && name.eq_ignore_ascii_case("size") {
                    Some(Lin::size())
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn numeric(&mut self) -> Option<Lin> {
        let start = self.index;
        let end = syntax::number_end(self.text.as_bytes(), start);
        if !self.text.as_bytes()[start..end].iter().any(u8::is_ascii_digit) {
            return None;
        }
        self.index = end;
        let value: f64 = self.text[start..end].parse().ok()?;
        if !value.is_finite() {
            return None;
        }
        if self.eat(b'%') {
            let mut lin = Lin::length();
            lin.pct = Some(value);
            return Some(lin);
        }
        if self.peek().is_some_and(|byte| byte.is_ascii_alphabetic()) {
            let unit = self.identifier().to_ascii_lowercase();
            if !UNITS.contains(&unit.as_str()) {
                return None;
            }
            let (unit, value) = match absolute_factor(&unit) {
                Some(factor) => (String::from("px"), value * factor),
                None => (unit, value),
            };
            let mut lin = Lin::length();
            lin.units.push((unit, value));
            return Some(lin);
        }
        Some(Lin::number(value))
    }
}

#[derive(Clone, Debug, PartialEq)]
enum BasisSpec {
    Any,
    Keyword(Basis),
    Nested(Box<Spec>),
    Length(Lin),
}

#[derive(Clone, Debug, PartialEq)]
struct Spec {
    basis: BasisSpec,
    expr: Lin,
}

struct Flat {
    basis: Option<Basis>,
    lin: Lin,
    nest: u8,
}

fn keyword_basis(name: &str, kind: Kind) -> Option<Option<Basis>> {
    Some(match name.to_ascii_lowercase().as_str() {
        "any" => None,
        "auto" if kind != Kind::Max => Some(Basis::Auto),
        "min-content" => Some(Basis::MinContent),
        "max-content" => Some(Basis::MaxContent),
        "fit-content" => Some(Basis::FitContent),
        "stretch" => Some(Basis::Stretch),
        "content" if kind == Kind::FlexBasis => Some(Basis::Content),
        _ => return None,
    })
}

fn parse_spec_at(parser: &mut Parser, kind: Kind) -> Option<Spec> {
    if !parser.at_function("calc-size") {
        return None;
    }
    parser.depth += 1;
    if parser.depth > MAX_DEPTH {
        return None;
    }
    parser.index += "calc-size(".len();
    parser.skip_space();
    let basis = if parser.at_function("calc-size") {
        BasisSpec::Nested(Box::new(parse_spec_at(parser, kind)?))
    } else {
        let save = parser.index;
        let name = parser.identifier();
        let after = parser.peek();
        let boundary = after.is_none_or(|byte| byte == b',' || byte.is_ascii_whitespace());
        let keyword = if name.is_empty() || !boundary { None } else { keyword_basis(name, kind) };
        match keyword {
            Some(None) => BasisSpec::Any,
            Some(Some(basis)) => BasisSpec::Keyword(basis),
            None => {
                parser.index = save;
                let lin = parser.sum(false)?;
                if !lin.dim {
                    return None;
                }
                BasisSpec::Length(lin)
            }
        }
    };
    parser.skip_space();
    if !parser.eat(b',') {
        return None;
    }
    parser.skip_space();
    let expr = parser.sum(true)?;
    parser.skip_space();
    if !parser.eat(b')') || !expr.dim || !expr.finite() {
        return None;
    }
    if matches!(basis, BasisSpec::Any) && expr.size {
        return None;
    }
    parser.depth -= 1;
    Some(Spec { basis, expr })
}

fn parse_spec(text: &str, kind: Kind) -> Option<Spec> {
    let text = text.trim();
    if text.len() > MAX_INPUT {
        return None;
    }
    let mut parser = Parser { text, index: 0, depth: 0 };
    let spec = parse_spec_at(&mut parser, kind)?;
    (parser.index == text.len()).then_some(spec)
}

fn flatten(spec: &Spec) -> Flat {
    match &spec.basis {
        BasisSpec::Any => Flat { basis: None, lin: spec.expr.clone(), nest: 1 },
        BasisSpec::Keyword(basis) => Flat { basis: Some(*basis), lin: spec.expr.clone(), nest: 1 },
        BasisSpec::Length(length) => Flat { basis: None, lin: spec.expr.substitute(length), nest: 1 },
        BasisSpec::Nested(inner) => {
            let inner = flatten(inner);
            Flat { basis: inner.basis, lin: spec.expr.substitute(&inner.lin), nest: inner.nest.saturating_add(1) }
        }
    }
}

fn spec_text(spec: &Spec) -> String {
    let basis = match &spec.basis {
        BasisSpec::Any => String::from("any"),
        BasisSpec::Keyword(basis) => String::from(basis.keyword()),
        BasisSpec::Nested(inner) => spec_text(inner),
        BasisSpec::Length(length) => length.text(true),
    };
    format!("calc-size({basis}, {})", spec.expr.text(false))
}

fn flat_text(flat: &Flat) -> String {
    match flat.basis {
        Some(basis) => format!("calc-size({}, {})", basis.keyword(), flat.lin.text(false)),
        None if flat.lin.pct.is_none() && !flat.lin.size => format!("calc-size(any, {})", flat.lin.text(false)),
        None => flat.lin.text(true),
    }
}

fn parse_fit_content_argument(raw: &str) -> Option<Lin> {
    let raw = raw.trim();
    if raw.len() > MAX_INPUT {
        return None;
    }
    let mut parser = Parser { text: raw, index: 0, depth: 0 };
    if !parser.at_function("fit-content") {
        return None;
    }
    parser.index += "fit-content(".len();
    parser.skip_space();
    let start = parser.index;
    let lin = parser.sum(false)?;
    let argument = &raw[start..parser.index];
    parser.skip_space();
    if !parser.eat(b')') || parser.index != raw.len() || !lin.dim || !lin.finite() || negative_length_primitive(argument) {
        return None;
    }
    Some(lin)
}

fn starts_fit_content_function(raw: &str) -> bool {
    let raw = raw.trim_start().as_bytes();
    raw.len() > "fit-content(".len() && raw[.."fit-content".len()].eq_ignore_ascii_case(b"fit-content") && raw["fit-content".len()] == b'('
}

fn starts_calc_size(raw: &str) -> bool {
    let raw = raw.trim_start().as_bytes();
    raw.len() > "calc-size(".len() && raw[.."calc-size".len()].eq_ignore_ascii_case(b"calc-size") && raw["calc-size".len()] == b'('
}

/// `Some(None)` is an invalid `calc-size()` for a sizing property; `None`
/// means the declaration is not a `calc-size()` one.
pub(super) fn parse_declaration(name: &str, raw: &str) -> Option<Option<Value>> {
    let (kind, slot) = property(name)?;
    if starts_fit_content_function(raw) {
        return Some(parse_fit_content_argument(raw).map(|lin| Value::CalcSize(slot, fit_content_text(&lin).into())));
    }
    if !starts_calc_size(raw) {
        return None;
    }
    Some(declaration_value(kind, slot, raw))
}

fn any_length(slot: usize, lin: &Lin) -> bool {
    matches!(slot, 3 | 4 | 23 | 24 | 51 | 52) && lin.pct.is_none() && !lin.size
}

fn declaration_value(kind: Kind, slot: usize, raw: &str) -> Option<Value> {
    let flat = flatten(&parse_spec(raw, kind)?);
    match flat.basis {
        None if any_length(slot, &flat.lin) => {
            let text = format!("calc-size(any, {})", flat.lin.text(false));
            Some(Value::CalcSize(slot, text.into()))
        }
        None => parse_context_length(slot, &flat.lin.calc_text(), true),
        Some(basis) => {
            let text = nested_text(basis.keyword(), &flat.lin.text(false), flat.nest);
            Some(Value::CalcSize(slot, text.into()))
        }
    }
}

fn fit_content_text(lin: &Lin) -> String {
    format!("fit-content({})", lin.text(true))
}

pub(super) fn specified(name: &str, raw: &str) -> Option<String> {
    let (kind, _) = property(name)?;
    if starts_fit_content_function(raw) {
        return parse_fit_content_argument(raw).map(|lin| fit_content_text(&lin));
    }
    Some(spec_text(&parse_spec(raw, kind)?))
}

/// Whether the text is a `calc-size()` or `fit-content()` value that
/// resolves to an intrinsic sizing keyword rather than a plain length.
pub(super) fn is_intrinsic_text(text: &str) -> bool {
    starts_calc_size(text) || starts_fit_content_function(text)
}

pub(super) fn uses_container_units(text: &str) -> bool {
    let units = if starts_fit_content_function(text) {
        parse_fit_content_argument(text).map(|lin| lin.units)
    } else {
        parse_spec(text, Kind::FlexBasis).map(|spec| flatten(&spec).lin.units)
    };
    units.is_some_and(|units| units.iter().any(|(unit, _)| unit.starts_with("cq")))
}

pub(super) fn applies(name: &str, raw: &str) -> bool {
    property(name).is_some() && (starts_calc_size(raw) || starts_fit_content_function(raw))
}

/// Freeze a keyword-basis `calc-size()` against the element's length context.
pub(super) fn resolve(slot: usize, text: &str, context: LengthContext, query: ContainerUnitContext) -> Option<Value> {
    kind_of_slot(slot)?;
    let (basis, lin, nest) = if starts_fit_content_function(text) {
        let mut lin = parse_fit_content_argument(text)?;
        lin.k = 1.0;
        (Basis::FitContentArgument, lin, 1)
    } else {
        let flat = flatten(&parse_spec(text, kind_of_slot(slot)?)?);
        let basis = match flat.basis {
            Some(basis) => basis,
            None if any_length(slot, &flat.lin) => Basis::Any,
            None => return None,
        };
        (basis, flat.lin, flat.nest)
    };
    let mut px = 0.0f32;
    for (unit, value) in &lin.units {
        px += contextual_length_with_query(&format!("{}{unit}", number_text(*value)), Some(context), true, query)?;
    }
    let value = CalcSizeValue {
        basis,
        px,
        frac: (lin.pct.unwrap_or(0.0) / 100.0) as f32,
        k: lin.k as f32,
        size_term: lin.size && basis != Basis::FitContentArgument,
        px_term: !lin.units.is_empty(),
        nest,
    };
    let sizing = IntrinsicSizing::CalcSize(value);
    Some(if slot == 20 { Value::FlexBasisIntrinsic(sizing) } else { Value::IntrinsicSize(slot, sizing) })
}

struct Side {
    basis: Option<Basis>,
    lin: Lin,
    nest: u8,
}

fn side(text: &str, allow_keywords: bool) -> Option<Side> {
    let text = text.trim();
    if starts_calc_size(text) {
        let flat = flatten(&parse_spec(text, Kind::FlexBasis)?);
        return Some(Side { basis: flat.basis, lin: flat.lin, nest: flat.nest });
    }
    if allow_keywords {
        if let Some(Some(basis)) = keyword_basis(text, Kind::Preferred) {
            return Some(Side { basis: Some(basis), lin: Lin::size(), nest: 1 });
        }
    }
    if text.len() > MAX_INPUT {
        return None;
    }
    let mut parser = Parser { text, index: 0, depth: 0 };
    let lin = parser.sum(false)?;
    (parser.index == text.len() && lin.dim).then_some(Side { basis: None, lin, nest: 1 })
}

/// Weighted sum of sizing values where at least one operand is a
/// `calc-size()` (or, with `allow_keywords`, an intrinsic keyword). Operands
/// with different keyword bases do not combine.
pub fn combine(values: &[(&str, f64)], allow_keywords: bool) -> Option<String> {
    let has_function = |text: &str| {
        text.len() <= MAX_INPUT
            && text.as_bytes().windows("calc-size(".len()).any(|window| window.eq_ignore_ascii_case(b"calc-size("))
    };
    let functional = values.iter().any(|(text, _)| has_function(text));
    let keywords = allow_keywords || functional;
    if !functional
        && !(allow_keywords
            && values.iter().any(|(text, _)| keyword_basis(text.trim(), Kind::Preferred).is_some_and(|basis| basis.is_some())))
    {
        return None;
    }
    let mut basis: Option<Basis> = None;
    let mut total = Lin::length();
    let mut percentage_basis = false;
    for &(text, weight) in values {
        if !weight.is_finite() {
            return None;
        }
        let side = side(text, keywords)?;
        if side.nest > MAX_INTERPOLATED_NEST {
            return None;
        }
        percentage_basis |= side.basis.is_none() && side.lin.pct.is_some() && starts_calc_size(text.trim());
        match (basis, side.basis) {
            (Some(left), Some(right)) if left != right => return None,
            (None, Some(right)) => basis = Some(right),
            _ => {}
        }
        total = total.add(&side.lin.scaled(weight))?;
    }
    if !total.finite() || percentage_basis && basis.is_some() {
        return None;
    }
    Some(flat_text(&Flat { basis, lin: total, nest: 1 }))
}

/// Whether either value is a `calc-size()` expression; such values that do not combine
/// are not interpolable at all.
pub(crate) fn involves_function(values: &[&str]) -> bool {
    values.iter().any(|text| {
        text.len() <= MAX_INPUT
            && text.as_bytes().windows("calc-size(".len()).any(|window| window.eq_ignore_ascii_case(b"calc-size("))
    })
}

pub(crate) fn interpolate(from: &str, to: &str, progress: f64, allow_keywords: bool) -> Option<String> {
    combine(&[(from, 1.0 - progress), (to, progress)], allow_keywords)
}

/// Specified serialization of a unitless zero given to a sizing property.
pub(super) fn unitless_zero(name: &str, raw: &str) -> Option<String> {
    if !is_size_property(name) {
        return None;
    }
    let value = typed_numeric::parse_numeric_value(raw.trim())?;
    (value.unit == typed_numeric::NumericUnit::Number && value.value == 0.0).then(|| String::from("0px"))
}

pub fn is_size_property(name: &str) -> bool {
    property(name).is_some()
}

/// Interpolation of intrinsic size keywords against lengths, enabled by
/// `interpolate-size: allow-keywords` on the animated element.
pub fn interpolate_keywords(property: &str, from: &str, to: &str, progress: f64) -> Option<String> {
    if !is_size_property(property) || !progress.is_finite() {
        return None;
    }
    interpolate(from, to, progress, true)
}

impl Style {
    pub fn allows_size_keyword_interpolation(&self) -> bool {
        extra_props::computed(self, "interpolate-size").as_deref() == Some("allow-keywords")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialize(name: &str, raw: &str) -> Option<String> {
        specified(name, raw)
    }

    #[test]
    fn serializes_canonical_forms() {
        assert_eq!(
            serialize("height", "calc-size(min-content, size * 2)").as_deref(),
            Some("calc-size(min-content, 2 * size)")
        );
        assert_eq!(
            serialize("max-height", "calc-size(fit-content, 30px + size / 2)").as_deref(),
            Some("calc-size(fit-content, 30px + (0.5 * size))")
        );
        assert_eq!(
            serialize("width", "calc-size(any, 50px + 30%)").as_deref(),
            Some("calc-size(any, 30% + 50px)")
        );
    }

    #[test]
    fn rejects_invalid_forms() {
        assert!(serialize("width", "calc-size(any, size)").is_none());
        assert!(serialize("max-width", "calc-size(auto, size)").is_none());
        assert!(serialize("width", "calc-size(0, 0px)").is_none());
        assert!(serialize("width", "calc-size(content, size)").is_none());
    }

    #[test]
    fn any_basis_keeps_calc_size_identity() {
        let value = resolve(3, "calc-size(any, 50px)", static_length_context(), ContainerUnitContext::default());
        let Some(Value::IntrinsicSize(3, IntrinsicSizing::CalcSize(calc))) = value else { panic!("expected an any-basis value") };
        assert_eq!(calc.basis, Basis::Any);
        assert_eq!(calc.serialize(), "calc-size(any, 50px)");
        assert_eq!(
            interpolate("calc-size(any, 20px)", "calc-size(any, 60px)", 0.5, false).as_deref(),
            Some("calc-size(any, 40px)")
        );
    }

    #[test]
    fn deeply_nested_values_parse_but_do_not_interpolate() {
        let mut text = String::from("auto");
        for _ in 0..20 {
            text = format!("calc-size({text}, size / 2 + size / 2)");
        }
        assert!(serialize("height", &text).is_some());
        assert!(interpolate(&text, "calc-size(auto, size * 3)", 0.5, false).is_none());
        let shallow = "calc-size(calc-size(auto, size / 2), size * 2)";
        assert!(interpolate(shallow, "calc-size(auto, size * 5)", 0.5, false).is_some());
    }

    #[test]
    fn interpolates_affine_values() {
        let value = interpolate("calc-size(auto, size)", "calc-size(auto, size * 2)", 0.5, false);
        assert_eq!(value.as_deref(), Some("calc-size(auto, 1.5 * size)"));
    }
}
