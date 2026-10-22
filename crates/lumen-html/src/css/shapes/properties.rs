use super::basic::{is_shape_function, mix_shapes, parse_shape, Allow, Op, Shape};
use super::geom::*;
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Prop {
    ClipPath,
    ShapeOutside,
    ShapeMargin,
    ShapeImageThreshold,
    OffsetPath,
    OffsetDistance,
    OffsetRotate,
    OffsetAnchor,
    OffsetPosition,
    Clip,
    ObjectViewBox,
    BorderShape,
}

impl Prop {
    fn default_box(self) -> &'static str {
        match self {
            Prop::ShapeOutside => "margin-box",
            _ => "border-box",
        }
    }

    fn boxes(self) -> &'static [&'static str] {
        match self {
            Prop::ClipPath => &["margin-box", "border-box", "padding-box", "content-box", "fill-box", "stroke-box", "view-box"],
            Prop::ShapeOutside => &["margin-box", "border-box", "padding-box", "content-box"],
            Prop::ObjectViewBox => &[],
            Prop::BorderShape => &["half-border-box", "margin-box", "border-box", "padding-box", "content-box", "fill-box", "stroke-box", "view-box"],
            _ => &["border-box", "padding-box", "content-box", "fill-box", "stroke-box", "view-box"],
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Source {
    None,
    Url(String),
    Image { specified: String, computed: String },
    Shape(Shape),
    BoxOnly,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct PathValue {
    pub source: Source,
    pub bx: Option<&'static str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RotKeyword {
    Auto,
    Reverse,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Rot {
    pub keyword: Option<RotKeyword>,
    pub angle: Option<Angle>,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Place {
    Normal,
    Auto,
    Pos(Pos),
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Parsed {
    Path(PathValue),
    Len(Len),
    Number(f64),
    Rotate(Rot),
    Place(Place),
    Clip(Option<[Option<Len>; 4]>),
    Border(BorderShape),
}

fn box_keyword(prop: Prop, token: &str) -> Option<&'static str> {
    let keyword = lower(token);
    prop.boxes().iter().copied().find(|candidate| *candidate == keyword.as_ref())
}

fn url_text(url: &str) -> String {
    alloc::format!("url({})", serialize_css_string(url))
}

fn parse_source(prop: Prop, token: &str, env: Env) -> Option<Source> {
    if prop == Prop::ObjectViewBox {
        let (name, _) = split_function(token)?;
        if !matches!(name.as_str(), "inset" | "rect" | "xywh") {
            return None;
        }
    }
    if is_shape_function(token) {
        let allow = Allow { ray: prop == Prop::OffsetPath, path_fill: prop != Prop::OffsetPath };
        return parse_shape(token, env, allow).map(Source::Shape);
    }
    if prop == Prop::ShapeOutside {
        let (name, _) = split_function(token)?;
        if !matches!(name.as_str(), "linear-gradient" | "repeating-linear-gradient" | "radial-gradient" | "repeating-radial-gradient"
            | "conic-gradient" | "repeating-conic-gradient" | "cross-fade" | "image-set" | "-webkit-image-set" | "image" | "element" | "paint" | "url" | "src") {
            return None;
        }
        let specified = image_source::serialize(token)?;
        let computed = image_source::computed(token, env.ctx, env.query, &legacy_source_color(Style::initial().color), None)?;
        return Some(Source::Image { specified, computed });
    }
    background_url(token).map(|url| Source::Url(url.to_string()))
}

fn parse_path_like(prop: Prop, raw: &str, env: Env) -> Option<PathValue> {
    let tokens = components(raw.trim())?;
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    if tokens.len() == 1 && lower(tokens[0]) == "none" {
        return Some(PathValue { source: Source::None, bx: None });
    }
    let mut source = None;
    let mut bx = None;
    for token in tokens {
        if let Some(found) = box_keyword(prop, token) {
            if bx.is_some() {
                return None;
            }
            bx = Some(found);
            continue;
        }
        if source.is_some() {
            return None;
        }
        source = Some(parse_source(prop, token, env)?);
    }
    let source = match source {
        Some(Source::Url(_) | Source::Image { .. }) if bx.is_some() && prop != Prop::OffsetPath => return None,
        Some(source) => source,
        None => Source::BoxOnly,
    };
    Some(PathValue { source, bx })
}

fn parse_rotate(raw: &str, env: Env) -> Option<Rot> {
    let tokens = components(raw.trim())?;
    if tokens.is_empty() || tokens.len() > 2 {
        return None;
    }
    let mut keyword = None;
    let mut angle = None;
    for token in tokens {
        match lower(token).as_ref() {
            "auto" | "reverse" if keyword.is_none() => {
                keyword = Some(if token.eq_ignore_ascii_case("auto") { RotKeyword::Auto } else { RotKeyword::Reverse });
            }
            _ if angle.is_none() => angle = Some(parse_angle(token, env)?),
            _ => return None,
        }
    }
    Some(Rot { keyword, angle })
}

fn parse_place(prop: Prop, raw: &str, env: Env) -> Option<Place> {
    let tokens = components(raw.trim())?;
    if tokens.len() == 1 {
        match lower(tokens[0]).as_ref() {
            "auto" => return Some(Place::Auto),
            "normal" if prop == Prop::OffsetPosition => return Some(Place::Normal),
            _ => {}
        }
    }
    parse_position(&tokens, env).map(Place::Pos)
}

fn parse_number(raw: &str) -> Option<f64> {
    let expression = typed_transforms::argument_expression(raw.trim(), typed_transforms::TransformArgument::Scale)?;
    typed_transforms::resolved_argument_scalar(&expression, None)
}

fn parse_clip(raw: &str, env: Env) -> Option<Option<[Option<Len>; 4]>> {
    let raw = raw.trim();
    if lower(raw) == "auto" {
        return Some(None);
    }
    let (name, inner) = split_function(raw)?;
    if name != "rect" {
        return None;
    }
    let comma = top_level_split(inner, b',', 5)?;
    let parts: Vec<&str> = if comma.len() == 4 {
        comma
    } else if comma.len() == 1 {
        components(inner)?
    } else {
        return None;
    };
    if parts.len() != 4 {
        return None;
    }
    let mut sides: [Option<Len>; 4] = [None, None, None, None];
    for (slot, part) in sides.iter_mut().zip(parts) {
        *slot = if lower(part) == "auto" { None } else { Some(parse_length_only(part, env, false)?) };
    }
    Some(Some(sides))
}

pub(super) fn parse(prop: Prop, raw: &str, env: Env) -> Option<Parsed> {
    Some(match prop {
        Prop::BorderShape => Parsed::Border(parse_border_shape(raw, env)?),
        Prop::ClipPath | Prop::ShapeOutside | Prop::OffsetPath | Prop::ObjectViewBox => Parsed::Path(parse_path_like(prop, raw, env)?),
        Prop::ShapeMargin => Parsed::Len(parse_len(raw, env, true)?),
        Prop::OffsetDistance => Parsed::Len(parse_len(raw, env, false)?),
        Prop::ShapeImageThreshold => Parsed::Number(parse_number(raw)?),
        Prop::OffsetRotate => Parsed::Rotate(parse_rotate(raw, env)?),
        Prop::OffsetAnchor | Prop::OffsetPosition => Parsed::Place(parse_place(prop, raw, env)?),
        Prop::Clip => Parsed::Clip(parse_clip(raw, env)?),
    })
}

impl PathValue {
    fn text(&self, prop: Prop, form: Form) -> String {
        let mut out = match &self.source {
            Source::None => return String::from("none"),
            Source::Url(url) => url_text(url),
            Source::Image { specified, computed } => {
                if form == Form::Specified {
                    specified.clone()
                } else {
                    computed.clone()
                }
            }
            Source::Shape(shape) => shape.text(form),
            Source::BoxOnly => String::new(),
        };
        if let Some(bx) = self.bx {
            if self.source == Source::BoxOnly || bx != prop.default_box() {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(bx);
            }
        }
        out
    }

    fn compute(&self, prop: Prop) -> PathValue {
        let source = match &self.source {
            Source::Shape(Shape::Path { evenodd, data }) if prop == Prop::ShapeOutside => {
                Source::Shape(Shape::Path { evenodd: *evenodd, data: data.absolute() })
            }
            Source::Shape(shape) => Source::Shape(shape.compute()),
            other => other.clone(),
        };
        PathValue { source, bx: self.bx }
    }

    fn effective_box(&self, prop: Prop) -> &'static str {
        self.bx.unwrap_or(prop.default_box())
    }
}

impl Rot {
    fn computed(&self) -> Rot {
        let mut degrees = self.angle.as_ref().map_or(0.0, |angle| angle.deg);
        if self.keyword == Some(RotKeyword::Reverse) {
            degrees += 180.0;
        }
        Rot { keyword: self.keyword.map(|_| RotKeyword::Auto), angle: Some(Angle::degrees(degrees)) }
    }

    fn text(&self, form: Form) -> String {
        if form == Form::Computed {
            let computed = self.computed();
            let degrees = computed.angle.as_ref().map_or(0.0, |angle| angle.deg);
            let angle = Angle::degrees(degrees).text(Form::Computed);
            return match computed.keyword {
                Some(_) if degrees == 0.0 => String::from("auto"),
                Some(_) => alloc::format!("auto {angle}"),
                None => angle,
            };
        }
        let mut parts = Vec::new();
        match self.keyword {
            Some(RotKeyword::Auto) => parts.push(String::from("auto")),
            Some(RotKeyword::Reverse) => parts.push(String::from("reverse")),
            None => {}
        }
        if let Some(angle) = &self.angle {
            parts.push(angle.text(form));
        }
        parts.join(" ")
    }
}

impl Place {
    fn text(&self, form: Form) -> String {
        match self {
            Place::Normal => String::from("normal"),
            Place::Auto => String::from("auto"),
            Place::Pos(pos) => pos.text(form),
        }
    }
}

fn clip_text(clip: &Option<[Option<Len>; 4]>, form: Form) -> String {
    match clip {
        None => String::from("auto"),
        Some(sides) => {
            let parts: Vec<String> =
                sides.iter().map(|side| side.as_ref().map_or_else(|| String::from("auto"), |len| len.text(form))).collect();
            alloc::format!("rect({})", parts.join(", "))
        }
    }
}

impl Parsed {
    pub(super) fn text(&self, prop: Prop, form: Form) -> String {
        match self {
            Parsed::Path(value) => value.text(prop, form),
            Parsed::Len(len) => len.text(form),
            Parsed::Number(number) => num(*number),
            Parsed::Rotate(rot) => rot.text(form),
            Parsed::Place(place) => place.text(form),
            Parsed::Clip(clip) => clip_text(clip, form),
            Parsed::Border(value) => value.text(form),
        }
    }

    pub(super) fn compute(&self, prop: Prop) -> Parsed {
        match self {
            Parsed::Path(value) => Parsed::Path(value.compute(prop)),
            Parsed::Len(len) => Parsed::Len(len.computed()),
            Parsed::Number(number) => Parsed::Number(if prop == Prop::ShapeImageThreshold { number.clamp(0.0, 1.0) } else { *number }),
            Parsed::Rotate(rot) => Parsed::Rotate(rot.computed()),
            Parsed::Place(Place::Pos(pos)) => Parsed::Place(Place::Pos(pos.computed())),
            Parsed::Place(place) => Parsed::Place(place.clone()),
            Parsed::Clip(clip) => Parsed::Clip(clip.as_ref().map(|sides| sides.each_ref().map(|side| side.as_ref().map(Len::computed)))),
            Parsed::Border(value) => Parsed::Border(value.compute()),
        }
    }
}

pub(super) fn mix(prop: Prop, a: &Parsed, b: &Parsed, op: Op) -> Option<Parsed> {
    let apply = |x: f64, y: f64| op.apply(x, y);
    match (a, b) {
        (Parsed::Path(a), Parsed::Path(b)) => {
            if a.effective_box(prop) != b.effective_box(prop) {
                return None;
            }
            match (&a.source, &b.source) {
                (Source::Shape(left), Source::Shape(right)) => {
                    Some(Parsed::Path(PathValue { source: Source::Shape(mix_shapes(left, right, op)?), bx: a.bx.or(b.bx) }))
                }
                _ => None,
            }
        }
        (Parsed::Len(a), Parsed::Len(b)) => {
            let mut mixed = a.map2(b, &apply);
            if prop == Prop::ShapeMargin {
                mixed.clamp_nonnegative();
            }
            Some(Parsed::Len(mixed))
        }
        (Parsed::Number(a), Parsed::Number(b)) => Some(Parsed::Number(op.apply(*a, *b).clamp(0.0, 1.0))),
        (Parsed::Rotate(a), Parsed::Rotate(b)) => {
            let (a, b) = (a.computed(), b.computed());
            if a.keyword != b.keyword {
                return None;
            }
            let degrees = op.apply(a.angle.as_ref()?.deg, b.angle.as_ref()?.deg);
            Some(Parsed::Rotate(Rot { keyword: a.keyword, angle: Some(Angle::degrees(degrees)) }))
        }
        (Parsed::Place(Place::Pos(a)), Parsed::Place(Place::Pos(b))) => {
            let (ax, ay) = a.lens();
            let (bx, by) = b.lens();
            Some(Parsed::Place(Place::Pos(Pos::from_lens(ax.map2(&bx, &apply), ay.map2(&by, &apply)))))
        }
        (Parsed::Border(a), Parsed::Border(b)) => {
            let (a, b) = (a.expanded()?, b.expanded()?);
            if a[0].1 != b[0].1 || a[1].1 != b[1].1 {
                return None;
            }
            let outer = mix_shapes(&a[0].0, &b[0].0, op)?;
            let inner = mix_shapes(&a[1].0, &b[1].0, op)?;
            Some(Parsed::Border(BorderShape::pair(outer, a[0].1, inner, a[1].1).compute()))
        }
        (Parsed::Clip(Some(a)), Parsed::Clip(Some(b))) => {
            let mut sides: [Option<Len>; 4] = [None, None, None, None];
            for index in 0..4 {
                sides[index] = match (&a[index], &b[index]) {
                    (Some(x), Some(y)) => Some(x.map2(y, &apply)),
                    (None, None) => None,
                    _ => return None,
                };
            }
            Some(Parsed::Clip(Some(sides)))
        }
        _ => None,
    }
}

const HALF_BORDER_BOX: &str = "half-border-box";

/// `none | [ <basic-shape> <geometry-box>? ]{1,2}`; the two shapes are the outer and
/// inner edge of the border. Boxes are kept as written until computed.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct BorderShape {
    entries: Vec<(Shape, Option<&'static str>)>,
}

fn parse_border_shape(raw: &str, env: Env) -> Option<BorderShape> {
    let tokens = components(raw.trim())?;
    if tokens.len() == 1 && lower(tokens[0]) == "none" {
        return Some(BorderShape { entries: Vec::new() });
    }
    let mut entries: Vec<(Shape, Option<&'static str>)> = Vec::new();
    for token in tokens {
        if is_shape_function(token) {
            if entries.len() == 2 {
                return None;
            }
            let shape = parse_shape(token, env, Allow { ray: false, path_fill: true })?;
            entries.push((shape, None));
            continue;
        }
        let found = box_keyword(Prop::BorderShape, token)?;
        let last = entries.last_mut()?;
        if last.1.is_some() {
            return None;
        }
        last.1 = Some(found);
    }
    if entries.is_empty() {
        return None;
    }
    Some(BorderShape { entries })
}

impl BorderShape {
    fn pair(outer: Shape, outer_box: &'static str, inner: Shape, inner_box: &'static str) -> BorderShape {
        BorderShape { entries: alloc::vec![(outer, Some(outer_box)), (inner, Some(inner_box))] }
    }

    /// Outer and inner shape with their effective boxes; a lone shape serves as both.
    fn expanded(&self) -> Option<[(Shape, &'static str); 2]> {
        match self.entries.as_slice() {
            [(shape, bx)] => {
                let bx = bx.unwrap_or(HALF_BORDER_BOX);
                Some([(shape.clone(), bx), (shape.clone(), bx)])
            }
            [(outer, outer_box), (inner, inner_box)] => {
                Some([(outer.clone(), outer_box.unwrap_or("border-box")), (inner.clone(), inner_box.unwrap_or("padding-box"))])
            }
            _ => None,
        }
    }

    fn compute(&self) -> BorderShape {
        let Some([outer, inner]) = self.expanded() else { return self.clone() };
        let (outer_shape, inner_shape) = (outer.0.compute(), inner.0.compute());
        if outer_shape == inner_shape && outer.1 == inner.1 {
            return BorderShape { entries: alloc::vec![(outer_shape, Some(outer.1))] };
        }
        BorderShape::pair(outer_shape, outer.1, inner_shape, inner.1)
    }

    fn text(&self, form: Form) -> String {
        if self.entries.is_empty() {
            return String::from("none");
        }
        let canonical = form == Form::Computed;
        let defaults: &[&str] = if self.entries.len() == 1 { &[HALF_BORDER_BOX] } else { &["border-box", "padding-box"] };
        let mut parts = Vec::new();
        for (index, (shape, bx)) in self.entries.iter().enumerate() {
            parts.push(shape.text(form));
            if let Some(bx) = bx {
                if !canonical || *bx != defaults[index] {
                    parts.push(String::from(*bx));
                }
            }
        }
        parts.join(" ")
    }
}
