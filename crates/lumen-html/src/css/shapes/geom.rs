use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Form {
    Specified,
    Computed,
}

#[derive(Clone, Copy)]
pub(super) struct Env {
    pub ctx: LengthContext,
    pub query: ContainerUnitContext,
}

impl Env {
    pub(super) fn new(ctx: LengthContext) -> Self {
        Self { ctx, query: ContainerUnitContext::no_container(ctx.viewport) }
    }
}

pub(super) fn num(value: f64) -> String {
    if !value.is_finite() {
        return String::from("0");
    }
    let mut value = value;
    if value.abs() < 1e-6 {
        value = 0.0;
    }
    if value != 0.0 {
        let magnitude = libm::floor(libm::log10(value.abs())) as i32;
        let digits = 5 - magnitude;
        value = if digits >= 0 {
            let scale = libm::pow(10.0, f64::from(digits));
            libm::round(value * scale) / scale
        } else {
            let scale = libm::pow(10.0, f64::from(-digits));
            libm::round(value / scale) * scale
        };
    }
    if value == 0.0 {
        String::from("0")
    } else {
        alloc::format!("{value}")
    }
}

pub(super) use crate::css::extra_props::lower;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Len {
    pub spec: String,
    pub px: f64,
    pub pct: f64,
    pub has_pct: bool,
}

impl Len {
    pub(super) fn new(px: f64, pct: f64, has_pct: bool) -> Self {
        Self { spec: String::new(), px, pct, has_pct }
    }

    pub(super) fn zero() -> Self {
        Self::new(0.0, 0.0, false)
    }

    pub(super) fn percent(pct: f64) -> Self {
        Self::new(0.0, pct, true)
    }

    pub(super) fn pixels(px: f64) -> Self {
        Self::new(px, 0.0, false)
    }

    pub(super) fn is_zero(&self) -> bool {
        self.px == 0.0 && self.pct == 0.0
    }

    pub(super) fn percentage_aware(&self) -> bool {
        self.has_pct || self.pct != 0.0
    }

    pub(super) fn computed_text(&self) -> String {
        let px = num(self.px);
        if !self.percentage_aware() {
            return alloc::format!("{px}px");
        }
        let pct = num(self.pct);
        if px == "0" || px == "-0" {
            return alloc::format!("{pct}%");
        }
        let sign = if self.px < 0.0 { '-' } else { '+' };
        let magnitude = num(self.px.abs());
        alloc::format!("calc({pct}% {sign} {magnitude}px)")
    }

    pub(super) fn text(&self, form: Form) -> String {
        if form == Form::Specified && !self.spec.is_empty() {
            self.spec.clone()
        } else {
            self.computed_text()
        }
    }

    pub(super) fn map2(&self, other: &Len, op: &dyn Fn(f64, f64) -> f64) -> Len {
        Len::new(op(self.px, other.px), op(self.pct, other.pct), self.percentage_aware() || other.percentage_aware())
    }

    pub(super) fn minus(&self, other: &Len) -> Len {
        self.map2(other, &|a, b| a - b)
    }

    pub(super) fn from_full(&self) -> Len {
        Len::percent(100.0).minus(self)
    }

    pub(super) fn clamp_nonnegative(&mut self) {
        if self.pct == 0.0 {
            self.px = self.px.max(0.0);
        } else if self.px == 0.0 {
            self.pct = self.pct.max(0.0);
        }
    }

    pub(super) fn computed(&self) -> Len {
        Len::new(self.px, self.pct, self.has_pct)
    }
}

pub(super) fn parse_len(token: &str, env: Env, nonnegative: bool) -> Option<Len> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let expression = typed_transforms::argument_expression(token, typed_transforms::TransformArgument::LengthPercentage)?;
    let substituted = typed_numeric::substitute_sibling_functions(token, 1, 1)?.0;
    let resolved = transform_length_with_query(&substituted, env.ctx, env.query)?;
    if nonnegative && !math_function(token) && (resolved.pixels < 0.0 || resolved.percent < 0.0) {
        return None;
    }
    let spec = expression.serialize_specified()?;
    let mut len = Len {
        spec,
        px: f64::from(resolved.pixels),
        pct: f64::from(resolved.percent),
        has_pct: token.contains('%'),
    };
    if nonnegative {
        len.clamp_nonnegative();
    }
    Some(len)
}

pub(super) fn parse_length_only(token: &str, env: Env, nonnegative: bool) -> Option<Len> {
    let token = token.trim();
    typed_transforms::argument_expression(token, typed_transforms::TransformArgument::Length)?;
    parse_len(token, env, nonnegative).filter(|len| len.pct == 0.0)
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Angle {
    pub spec: String,
    pub deg: f64,
}

impl Angle {
    pub(super) fn degrees(deg: f64) -> Self {
        Self { spec: String::new(), deg }
    }

    pub(super) fn text(&self, form: Form) -> String {
        if form == Form::Specified && !self.spec.is_empty() {
            self.spec.clone()
        } else {
            alloc::format!("{}deg", num(self.deg))
        }
    }
}

pub(super) fn parse_angle(token: &str, env: Env) -> Option<Angle> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    if typed_numeric::parse_numeric_value(token).is_some_and(|value| value.unit == typed_numeric::NumericUnit::Number) {
        return None;
    }
    let expression = typed_transforms::argument_expression(token, typed_transforms::TransformArgument::Angle)?;
    let deg = typed_transforms::resolved_argument_scalar(&expression, None)
        .or_else(|| typed_transforms::resolved_angle_in_context(&expression, env.ctx, env.query))?;
    Some(Angle { spec: expression.serialize_specified()?, deg })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Near,
    Center,
    Far,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Axis {
    Kw(Side),
    Plain(Len),
    Edged(Side, Len),
}

impl Axis {
    pub(super) fn len(&self) -> Len {
        match self {
            Axis::Kw(Side::Near) => Len::percent(0.0),
            Axis::Kw(Side::Center) => Len::percent(50.0),
            Axis::Kw(Side::Far) => Len::percent(100.0),
            Axis::Plain(len) | Axis::Edged(Side::Near, len) => len.computed(),
            Axis::Edged(_, len) => len.from_full(),
        }
    }

    pub(super) fn text(&self, horizontal: bool, form: Form) -> String {
        if form == Form::Computed {
            return self.len().computed_text();
        }
        let keyword = |side: Side| match (side, horizontal) {
            (Side::Near, true) => "left",
            (Side::Near, false) => "top",
            (Side::Center, _) => "center",
            (Side::Far, true) => "right",
            (Side::Far, false) => "bottom",
        };
        match self {
            Axis::Kw(side) => String::from(keyword(*side)),
            Axis::Plain(len) => len.text(form),
            Axis::Edged(side, len) => alloc::format!("{} {}", keyword(*side), len.text(form)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Pos {
    pub x: Axis,
    pub y: Axis,
}

impl Pos {
    pub(super) fn center() -> Self {
        Pos { x: Axis::Kw(Side::Center), y: Axis::Kw(Side::Center) }
    }

    pub(super) fn from_lens(x: Len, y: Len) -> Self {
        Pos { x: Axis::Plain(x), y: Axis::Plain(y) }
    }

    pub(super) fn text(&self, form: Form) -> String {
        alloc::format!("{} {}", self.x.text(true, form), self.y.text(false, form))
    }

    pub(super) fn lens(&self) -> (Len, Len) {
        (self.x.len(), self.y.len())
    }

    pub(super) fn computed(&self) -> Pos {
        let (x, y) = self.lens();
        Pos::from_lens(x, y)
    }
}

enum Tok {
    X(Side),
    Y(Side),
    C,
    L(Len),
}

fn classify(token: &str, env: Env) -> Option<Tok> {
    Some(match lower(token).as_ref() {
        "left" => Tok::X(Side::Near),
        "right" => Tok::X(Side::Far),
        "top" => Tok::Y(Side::Near),
        "bottom" => Tok::Y(Side::Far),
        "center" => Tok::C,
        _ => Tok::L(parse_len(token, env, false)?),
    })
}

fn axis_of(token: Tok) -> Axis {
    match token {
        Tok::X(side) | Tok::Y(side) => Axis::Kw(side),
        Tok::C => Axis::Kw(Side::Center),
        Tok::L(len) => Axis::Plain(len),
    }
}

pub(super) fn parse_position(tokens: &[&str], env: Env) -> Option<Pos> {
    match tokens.len() {
        1 => Some(match classify(tokens[0], env)? {
            Tok::X(side) => Pos { x: Axis::Kw(side), y: Axis::Kw(Side::Center) },
            Tok::Y(side) => Pos { x: Axis::Kw(Side::Center), y: Axis::Kw(side) },
            Tok::C => Pos::center(),
            Tok::L(len) => Pos { x: Axis::Plain(len), y: Axis::Kw(Side::Center) },
        }),
        2 => {
            let first = classify(tokens[0], env)?;
            let second = classify(tokens[1], env)?;
            if matches!(first, Tok::X(_) | Tok::C | Tok::L(_)) && matches!(second, Tok::Y(_) | Tok::C | Tok::L(_)) {
                Some(Pos { x: axis_of(first), y: axis_of(second) })
            } else if matches!(first, Tok::Y(_) | Tok::C) && matches!(second, Tok::X(_) | Tok::C) {
                Some(Pos { x: axis_of(second), y: axis_of(first) })
            } else {
                None
            }
        }
        4 => {
            let first = classify(tokens[0], env)?;
            let first_offset = parse_len(tokens[1], env, false)?;
            let second = classify(tokens[2], env)?;
            let second_offset = parse_len(tokens[3], env, false)?;
            match (first, second) {
                (Tok::X(a), Tok::Y(b)) => Some(Pos { x: Axis::Edged(a, first_offset), y: Axis::Edged(b, second_offset) }),
                (Tok::Y(a), Tok::X(b)) => Some(Pos { x: Axis::Edged(b, second_offset), y: Axis::Edged(a, first_offset) }),
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn is_position_token(token: &str, env: Env) -> bool {
    classify(token, env).is_some()
}

pub(super) fn split_function(token: &str) -> Option<(String, &str)> {
    let token = token.trim();
    let open = token.find('(')?;
    if !token.ends_with(')') {
        return None;
    }
    let name = &token[..open];
    if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') {
        return None;
    }
    let end = matching_css_block(token, open)?;
    if end != token.len() {
        return None;
    }
    Some((name.to_ascii_lowercase(), &token[open + 1..token.len() - 1]))
}
