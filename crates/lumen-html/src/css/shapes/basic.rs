use super::geom::*;
use super::paths::PathData;
use super::shape_fn::ShapeFn;
use super::*;

const MAX_POINTS: usize = 16384;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Radii {
    pub h: [Len; 4],
    pub v: [Len; 4],
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Radius {
    ClosestSide,
    FarthestSide,
    ClosestCorner,
    FarthestCorner,
    Len(Len),
}

impl Radius {
    fn text(&self, form: Form) -> String {
        match self {
            Radius::ClosestSide => String::from("closest-side"),
            Radius::FarthestSide => String::from("farthest-side"),
            Radius::ClosestCorner => String::from("closest-corner"),
            Radius::FarthestCorner => String::from("farthest-corner"),
            Radius::Len(len) => len.text(form),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RaySize {
    ClosestSide,
    ClosestCorner,
    FarthestSide,
    FarthestCorner,
    Sides,
}

impl RaySize {
    fn text(self) -> &'static str {
        match self {
            RaySize::ClosestSide => "closest-side",
            RaySize::ClosestCorner => "closest-corner",
            RaySize::FarthestSide => "farthest-side",
            RaySize::FarthestCorner => "farthest-corner",
            RaySize::Sides => "sides",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Shape {
    Inset { sides: [Len; 4], radii: Option<Radii> },
    Rect { sides: [Option<Len>; 4], radii: Option<Radii> },
    Xywh { x: Len, y: Len, w: Len, h: Len, radii: Option<Radii> },
    Circle { r: Radius, at: Option<Pos> },
    Ellipse { rx: Radius, ry: Radius, at: Option<Pos> },
    Polygon { evenodd: bool, round: Option<Len>, points: Vec<(Len, Len)> },
    Path { evenodd: bool, data: PathData },
    Function(ShapeFn),
    Ray { angle: Angle, size: RaySize, contain: bool, at: Option<Pos> },
}

#[derive(Clone, Copy)]
pub(super) struct Allow {
    pub ray: bool,
    pub path_fill: bool,
}

pub(super) fn parse_shape(token: &str, env: Env, allow: Allow) -> Option<Shape> {
    let (name, inner) = split_function(token)?;
    match name.as_str() {
        "inset" => parse_inset(inner, env),
        "circle" => parse_circle(inner, env),
        "ellipse" => parse_ellipse(inner, env),
        "polygon" => parse_polygon(inner, env),
        "rect" => parse_rect(inner, env),
        "xywh" => parse_xywh(inner, env),
        "path" => super::paths::parse_function(inner, allow.path_fill),
        "shape" => super::shape_fn::parse(inner, env).map(Shape::Function),
        "ray" if allow.ray => parse_ray(inner, env),
        _ => None,
    }
}

pub(super) fn is_shape_function(token: &str) -> bool {
    split_function(token).is_some_and(|(name, _)| {
        matches!(name.as_str(), "inset" | "circle" | "ellipse" | "polygon" | "rect" | "xywh" | "path" | "shape" | "ray")
    })
}

fn expand4<T: Clone>(values: &[T]) -> Option<[T; 4]> {
    match values {
        [a] => Some([a.clone(), a.clone(), a.clone(), a.clone()]),
        [a, b] => Some([a.clone(), b.clone(), a.clone(), b.clone()]),
        [a, b, c] => Some([a.clone(), b.clone(), c.clone(), b.clone()]),
        [a, b, c, d] => Some([a.clone(), b.clone(), c.clone(), d.clone()]),
        _ => None,
    }
}

fn parse_lens(tokens: &[&str], env: Env, nonnegative: bool) -> Option<Vec<Len>> {
    tokens.iter().map(|token| parse_len(token, env, nonnegative)).collect()
}

fn parse_radii(rest: &str, env: Env) -> Option<Radii> {
    let parts = top_level_split(rest, b'/', 3)?;
    if !(1..=2).contains(&parts.len()) {
        return None;
    }
    let horizontal = expand4(&parse_lens(&components(parts[0])?, env, true)?)?;
    let vertical = match parts.get(1) {
        Some(part) => expand4(&parse_lens(&components(part)?, env, true)?)?,
        None => horizontal.clone(),
    };
    Some(Radii { h: horizontal, v: vertical })
}

fn split_round<'a>(tokens: &'a [&'a str]) -> (&'a [&'a str], Option<&'a [&'a str]>) {
    match tokens.iter().position(|token| token.eq_ignore_ascii_case("round")) {
        Some(index) => (&tokens[..index], Some(&tokens[index + 1..])),
        None => (tokens, None),
    }
}

fn parse_round(rest: Option<&[&str]>, env: Env) -> Option<Option<Radii>> {
    match rest {
        None => Some(None),
        Some(tokens) => {
            if tokens.is_empty() {
                return None;
            }
            parse_radii(&tokens.join(" "), env).map(Some)
        }
    }
}

fn parse_inset(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let (sides, round) = split_round(&tokens);
    if sides.is_empty() || sides.len() > 4 {
        return None;
    }
    let sides = expand4(&parse_lens(sides, env, false)?)?;
    Some(Shape::Inset { sides, radii: parse_round(round, env)? })
}

fn parse_rect(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let (sides, round) = split_round(&tokens);
    if sides.len() != 4 {
        return None;
    }
    let mut parsed: [Option<Len>; 4] = [None, None, None, None];
    for (slot, token) in parsed.iter_mut().zip(sides) {
        *slot = if lower(token) == "auto" { None } else { Some(parse_len(token, env, false)?) };
    }
    Some(Shape::Rect { sides: parsed, radii: parse_round(round, env)? })
}

fn parse_xywh(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let (sides, round) = split_round(&tokens);
    if sides.len() != 4 {
        return None;
    }
    Some(Shape::Xywh {
        x: parse_len(sides[0], env, false)?,
        y: parse_len(sides[1], env, false)?,
        w: parse_len(sides[2], env, true)?,
        h: parse_len(sides[3], env, true)?,
        radii: parse_round(round, env)?,
    })
}

fn parse_radius(token: &str, env: Env) -> Option<Radius> {
    Some(match lower(token).as_ref() {
        "closest-side" => Radius::ClosestSide,
        "farthest-side" => Radius::FarthestSide,
        "closest-corner" => Radius::ClosestCorner,
        "farthest-corner" => Radius::FarthestCorner,
        _ => Radius::Len(parse_len(token, env, true)?),
    })
}

fn split_at_keyword<'a>(tokens: &'a [&'a str]) -> (&'a [&'a str], Option<&'a [&'a str]>) {
    match tokens.iter().position(|token| token.eq_ignore_ascii_case("at")) {
        Some(index) => (&tokens[..index], Some(&tokens[index + 1..])),
        None => (tokens, None),
    }
}

fn parse_at(rest: Option<&[&str]>, env: Env) -> Option<Option<Pos>> {
    match rest {
        None => Some(None),
        Some(tokens) => parse_position(tokens, env).map(Some),
    }
}

fn parse_circle(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let (sizes, at) = split_at_keyword(&tokens);
    let r = match sizes {
        [] => Radius::ClosestSide,
        [token] => parse_radius(token, env)?,
        _ => return None,
    };
    Some(Shape::Circle { r, at: parse_at(at, env)? })
}

fn parse_ellipse(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let (sizes, at) = split_at_keyword(&tokens);
    let (rx, ry) = match sizes {
        [] => (Radius::ClosestSide, Radius::ClosestSide),
        [a, b] => (parse_radius(a, env)?, parse_radius(b, env)?),
        _ => return None,
    };
    Some(Shape::Ellipse { rx, ry, at: parse_at(at, env)? })
}

fn parse_polygon(inner: &str, env: Env) -> Option<Shape> {
    let chunks = top_level_split(inner, b',', MAX_POINTS)?;
    let mut evenodd = false;
    let mut round = None;
    let mut points = &chunks[..];
    let head = components(chunks[0])?;
    let starts_header = head.first().is_some_and(|token| {
        let token = lower(token);
        matches!(token.as_ref(), "nonzero" | "evenodd" | "round")
    });
    if starts_header {
        let mut index = 0;
        match lower(head[0]).as_ref() {
            "nonzero" => index = 1,
            "evenodd" => {
                evenodd = true;
                index = 1;
            }
            _ => {}
        }
        if head.get(index).is_some_and(|token| token.eq_ignore_ascii_case("round")) {
            round = Some(parse_length_only(head.get(index + 1)?, env, true)?);
            index += 2;
        }
        if index != head.len() {
            return None;
        }
        points = &chunks[1..];
    }
    if points.is_empty() {
        return None;
    }
    let mut parsed = Vec::new();
    for chunk in points {
        let pair = components(chunk)?;
        let [x, y] = pair.as_slice() else { return None };
        parsed.push((parse_len(x, env, false)?, parse_len(y, env, false)?));
    }
    Some(Shape::Polygon { evenodd, round, points: parsed })
}

fn parse_ray(inner: &str, env: Env) -> Option<Shape> {
    let tokens = components(inner)?;
    let mut angle = None;
    let mut size = None;
    let mut contain = false;
    let mut at = None;
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        let keyword = lower(token);
        match keyword.as_ref() {
            "at" => {
                if at.is_some() {
                    return None;
                }
                let mut end = index + 1;
                while end < tokens.len() && end - index - 1 < 4 && is_position_token(tokens[end], env) {
                    end += 1;
                }
                let span = &tokens[index + 1..end];
                let count = span.len();
                let usable = if count == 3 { 2 } else { count };
                at = Some(parse_position(&tokens[index + 1..index + 1 + usable], env)?);
                index += 1 + usable;
                continue;
            }
            "contain" => {
                if contain {
                    return None;
                }
                contain = true;
            }
            "closest-side" | "closest-corner" | "farthest-side" | "farthest-corner" | "sides" => {
                if size.is_some() {
                    return None;
                }
                size = Some(match keyword.as_ref() {
                    "closest-side" => RaySize::ClosestSide,
                    "closest-corner" => RaySize::ClosestCorner,
                    "farthest-side" => RaySize::FarthestSide,
                    "farthest-corner" => RaySize::FarthestCorner,
                    _ => RaySize::Sides,
                });
            }
            _ => {
                if angle.is_some() {
                    return None;
                }
                angle = Some(parse_angle(token, env)?);
            }
        }
        index += 1;
    }
    Some(Shape::Ray { angle: angle?, size: size.unwrap_or(RaySize::ClosestSide), contain, at })
}

fn radii_text(radii: &Radii, form: Form) -> Option<String> {
    if radii.h.iter().chain(radii.v.iter()).all(Len::is_zero) {
        return None;
    }
    let texts = |lens: &[Len; 4]| -> [String; 4] { [0, 1, 2, 3].map(|i| lens[i].text(form)) };
    let horizontal = texts(&radii.h);
    let vertical = texts(&radii.v);
    let collapse = |values: &[String; 4]| super::super::declaration_block::serialize_box_sides([&values[0], &values[1], &values[2], &values[3]]);
    if horizontal == vertical {
        Some(collapse(&horizontal))
    } else {
        Some(alloc::format!("{} / {}", collapse(&horizontal), collapse(&vertical)))
    }
}

fn sides_text(sides: &[Len; 4], form: Form) -> String {
    let texts: [String; 4] = [0, 1, 2, 3].map(|i| sides[i].text(form));
    super::super::declaration_block::serialize_box_sides([&texts[0], &texts[1], &texts[2], &texts[3]])
}

fn with_round(mut body: String, radii: &Option<Radii>, form: Form) -> String {
    if let Some(text) = radii.as_ref().and_then(|radii| radii_text(radii, form)) {
        body.push_str(" round ");
        body.push_str(&text);
    }
    body
}

fn at_text(at: &Option<Pos>, parts: &mut Vec<String>, form: Form) {
    if let Some(at) = at {
        parts.push(alloc::format!("at {}", at.text(form)));
    }
}

impl Shape {
    pub(super) fn text(&self, form: Form) -> String {
        match self {
            Shape::Inset { sides, radii } => {
                alloc::format!("inset({})", with_round(sides_text(sides, form), radii, form))
            }
            Shape::Rect { sides, radii } => {
                let body = sides
                    .iter()
                    .map(|side| side.as_ref().map_or_else(|| String::from("auto"), |len| len.text(form)))
                    .collect::<Vec<_>>()
                    .join(" ");
                alloc::format!("rect({})", with_round(body, radii, form))
            }
            Shape::Xywh { x, y, w, h, radii } => {
                let body = [x, y, w, h].iter().map(|len| len.text(form)).collect::<Vec<_>>().join(" ");
                alloc::format!("xywh({})", with_round(body, radii, form))
            }
            Shape::Circle { r, at } => {
                let mut parts = Vec::new();
                if *r != Radius::ClosestSide {
                    parts.push(r.text(form));
                }
                at_text(at, &mut parts, form);
                alloc::format!("circle({})", parts.join(" "))
            }
            Shape::Ellipse { rx, ry, at } => {
                let mut parts = Vec::new();
                if !(*rx == Radius::ClosestSide && *ry == Radius::ClosestSide) {
                    parts.push(rx.text(form));
                    parts.push(ry.text(form));
                }
                at_text(at, &mut parts, form);
                alloc::format!("ellipse({})", parts.join(" "))
            }
            Shape::Polygon { evenodd, round, points } => {
                let mut segments = Vec::new();
                let mut head = Vec::new();
                if *evenodd {
                    head.push(String::from("evenodd"));
                }
                if let Some(round) = round.as_ref().filter(|len| !len.is_zero()) {
                    head.push(alloc::format!("round {}", round.text(form)));
                }
                if !head.is_empty() {
                    segments.push(head.join(" "));
                }
                for (x, y) in points {
                    segments.push(alloc::format!("{} {}", x.text(form), y.text(form)));
                }
                alloc::format!("polygon({})", segments.join(", "))
            }
            Shape::Path { evenodd, data } => {
                let quoted = serialize_css_string(&data.text());
                if *evenodd {
                    alloc::format!("path(evenodd, {quoted})")
                } else {
                    alloc::format!("path({quoted})")
                }
            }
            Shape::Function(function) => function.text(form),
            Shape::Ray { angle, size, contain, at } => {
                let mut parts = Vec::new();
                parts.push(angle.text(form));
                if *size != RaySize::ClosestSide {
                    parts.push(String::from(size.text()));
                }
                if *contain {
                    parts.push(String::from("contain"));
                }
                at_text(at, &mut parts, form);
                alloc::format!("ray({})", parts.join(" "))
            }
        }
    }

    pub(super) fn compute(&self) -> Shape {
        let radii = |radii: &Option<Radii>| {
            radii.as_ref().map(|radii| Radii { h: radii.h.each_ref().map(Len::computed), v: radii.v.each_ref().map(Len::computed) })
        };
        match self {
            Shape::Inset { sides, radii: r } => Shape::Inset { sides: sides.each_ref().map(Len::computed), radii: radii(r) },
            Shape::Rect { sides, radii: r } => {
                let [top, right, bottom, left] = sides;
                let near = |side: &Option<Len>| side.as_ref().map_or_else(|| Len::percent(0.0), Len::computed);
                let far = |side: &Option<Len>| side.as_ref().map_or_else(|| Len::percent(0.0), Len::from_full);
                Shape::Inset { sides: [near(top), far(right), far(bottom), near(left)], radii: radii(r) }
            }
            Shape::Xywh { x, y, w, h, radii: r } => Shape::Inset {
                sides: [
                    y.computed(),
                    Len::percent(100.0).minus(x).minus(w),
                    Len::percent(100.0).minus(y).minus(h),
                    x.computed(),
                ],
                radii: radii(r),
            },
            Shape::Circle { r, at } => Shape::Circle { r: compute_radius(r), at: at.as_ref().map(Pos::computed) },
            Shape::Ellipse { rx, ry, at } => {
                Shape::Ellipse { rx: compute_radius(rx), ry: compute_radius(ry), at: at.as_ref().map(Pos::computed) }
            }
            Shape::Polygon { evenodd, round, points } => Shape::Polygon {
                evenodd: *evenodd,
                round: round.as_ref().map(Len::computed),
                points: points.iter().map(|(x, y)| (x.computed(), y.computed())).collect(),
            },
            Shape::Path { evenodd, data } => Shape::Path { evenodd: *evenodd, data: data.clone() },
            Shape::Function(function) => Shape::Function(function.compute()),
            Shape::Ray { angle, size, contain, at } => Shape::Ray {
                angle: Angle::degrees(angle.deg),
                size: *size,
                contain: *contain,
                at: at.as_ref().map(Pos::computed),
            },
        }
    }
}

fn compute_radius(radius: &Radius) -> Radius {
    match radius {
        Radius::Len(len) => Radius::Len(len.computed()),
        other => other.clone(),
    }
}

pub(super) fn mix_radius(a: &Radius, b: &Radius, op: Op) -> Option<Radius> {
    match (a, b) {
        (Radius::Len(a), Radius::Len(b)) => {
            let mut mixed = a.map2(b, &|x, y| op.apply(x, y));
            mixed.clamp_nonnegative();
            Some(Radius::Len(mixed))
        }
        _ => None,
    }
}

fn mix_pos(a: &Option<Pos>, b: &Option<Pos>, op: Op) -> Option<Option<Pos>> {
    if a.is_none() && b.is_none() {
        return Some(None);
    }
    let center = Pos::center();
    let (ax, ay) = a.as_ref().unwrap_or(&center).lens();
    let (bx, by) = b.as_ref().unwrap_or(&center).lens();
    let apply = |x: f64, y: f64| op.apply(x, y);
    Some(Some(Pos::from_lens(ax.map2(&bx, &apply), ay.map2(&by, &apply))))
}

fn mix_radii(a: &Option<Radii>, b: &Option<Radii>, op: Op) -> Option<Radii> {
    if a.is_none() && b.is_none() {
        return None;
    }
    let zero = Radii { h: [Len::zero(), Len::zero(), Len::zero(), Len::zero()], v: [Len::zero(), Len::zero(), Len::zero(), Len::zero()] };
    let a = a.as_ref().unwrap_or(&zero);
    let b = b.as_ref().unwrap_or(&zero);
    let apply = |x: f64, y: f64| op.apply(x, y);
    let mix = |a: &[Len; 4], b: &[Len; 4]| -> [Len; 4] {
        [0, 1, 2, 3].map(|i| {
            let mut mixed = a[i].map2(&b[i], &apply);
            mixed.clamp_nonnegative();
            mixed
        })
    };
    Some(Radii { h: mix(&a.h, &b.h), v: mix(&a.v, &b.v) })
}

#[derive(Clone, Copy, Debug)]
pub(super) enum Op {
    Interpolate(f64),
    Add,
    Accumulate(f64),
}

impl Op {
    pub(super) fn apply(self, a: f64, b: f64) -> f64 {
        match self {
            Op::Interpolate(progress) => lumen_common::numeric::lerp(a, b, progress),
            Op::Add => a + b,
            Op::Accumulate(count) => count * a + b,
        }
    }
}

pub(super) fn mix_shapes(a: &Shape, b: &Shape, op: Op) -> Option<Shape> {
    let apply = |x: f64, y: f64| op.apply(x, y);
    match (a, b) {
        (Shape::Inset { sides: sa, radii: ra }, Shape::Inset { sides: sb, radii: rb }) => Some(Shape::Inset {
            sides: [0, 1, 2, 3].map(|i| sa[i].map2(&sb[i], &apply)),
            radii: mix_radii(ra, rb, op),
        }),
        (Shape::Circle { r: ra, at: pa }, Shape::Circle { r: rb, at: pb }) => {
            Some(Shape::Circle { r: mix_radius(ra, rb, op)?, at: mix_pos(pa, pb, op)? })
        }
        (Shape::Ellipse { rx: xa, ry: ya, at: pa }, Shape::Ellipse { rx: xb, ry: yb, at: pb }) => Some(Shape::Ellipse {
            rx: mix_radius(xa, xb, op)?,
            ry: mix_radius(ya, yb, op)?,
            at: mix_pos(pa, pb, op)?,
        }),
        (Shape::Polygon { evenodd: ea, round: ra, points: pa }, Shape::Polygon { evenodd: eb, round: rb, points: pb }) => {
            if ea != eb || pa.len() != pb.len() {
                return None;
            }
            let zero = Len::zero();
            let round = match (ra, rb) {
                (None, None) => None,
                _ => {
                    let mut mixed = ra.as_ref().unwrap_or(&zero).map2(rb.as_ref().unwrap_or(&zero), &apply);
                    mixed.clamp_nonnegative();
                    Some(mixed)
                }
            };
            Some(Shape::Polygon {
                evenodd: *ea,
                round,
                points: pa.iter().zip(pb).map(|((ax, ay), (bx, by))| (ax.map2(bx, &apply), ay.map2(by, &apply))).collect(),
            })
        }
        (Shape::Ray { angle: aa, size: sa, contain: ca, at: pa }, Shape::Ray { angle: ab, size: sb, contain: cb, at: pb }) => {
            if sa != sb || ca != cb {
                return None;
            }
            Some(Shape::Ray { angle: Angle::degrees(op.apply(aa.deg, ab.deg)), size: *sa, contain: *ca, at: mix_pos(pa, pb, op)? })
        }
        (Shape::Path { evenodd: ea, data: da }, Shape::Path { evenodd: eb, data: db }) => {
            if ea != eb {
                return None;
            }
            Some(Shape::Path { evenodd: *ea, data: super::paths::mix(da, db, op)? })
        }
        (Shape::Function(fa), Shape::Function(fb)) => Some(Shape::Function(super::shape_fn::mix(fa, fb, op)?)),
        (Shape::Path { evenodd: ea, data }, Shape::Function(function)) => {
            let converted = data.to_shape_fn(*ea)?;
            Some(Shape::Function(super::shape_fn::mix(&converted, function, op)?))
        }
        (Shape::Function(function), Shape::Path { evenodd: eb, data }) => {
            let converted = data.to_shape_fn(*eb)?;
            Some(Shape::Function(super::shape_fn::mix(function, &converted, op)?))
        }
        _ => None,
    }
}
