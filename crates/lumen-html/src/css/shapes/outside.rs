//! The float area of `shape-outside`: a resolved geometry in the float's
//! margin-box coordinate space that answers how far a line box band is pushed
//! away from the float (CSS Shapes 1 §2 and §3).
use super::basic::{Radii, Radius, Shape};
use super::geom::*;
use super::properties::{parse, Parsed, Prop, Source};
use super::*;

const LAYOUT_UNIT: f32 = 64.0;
const SAMPLE_STEP: f32 = 0.25;
const MAX_SAMPLES: usize = 512;

/// The used box metrics of a float needed to resolve the reference box.
pub(crate) struct FloatBox {
    pub width: f32,
    pub height: f32,
    pub margin: [f32; 4],
    pub border: [f32; 4],
    pub padding: [f32; 4],
}

/// A raster float area: one horizontal span per row of `rows`, starting at the
/// margin-box coordinate `top` with `row_height` per row.
pub(crate) struct RasterRows {
    pub top: f32,
    pub row_height: f32,
    pub rows: Vec<Option<(f32, f32)>>,
}

#[derive(Clone)]
struct RoundRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    radii: [[f32; 2]; 4],
}

#[derive(Clone)]
enum Geometry {
    Round(RoundRect),
    Ellipse { cx: f32, cy: f32, rx: f32, ry: f32 },
    Polygon { edges: Vec<[f32; 4]> },
    Raster(Arc<RasterRows>),
}

#[derive(Clone)]
pub(crate) struct FloatShape {
    width: f32,
    height: f32,
    margin: f32,
    geometry: Geometry,
}

fn resolve(len: &Len, basis: f32) -> f32 {
    (len.px + len.pct / 100.0 * f64::from(basis)) as f32
}

fn scale_radii(width: f32, height: f32, radii: &mut [[f32; 2]; 4]) {
    let mut scale = 1.0f32;
    for (a, b, axis, basis) in [(0, 1, 0, width), (3, 2, 0, width), (0, 3, 1, height), (1, 2, 1, height)] {
        let sum = radii[a][axis] + radii[b][axis];
        if sum > 0.0 {
            scale = scale.min(basis / sum);
        }
    }
    if scale < 1.0 && scale.is_finite() {
        for corner in radii.iter_mut() {
            corner[0] *= scale.max(0.0);
            corner[1] *= scale.max(0.0);
        }
    }
}

fn resolve_radii(radii: &Option<Radii>, width: f32, height: f32) -> [[f32; 2]; 4] {
    let mut out = [[0.0f32; 2]; 4];
    if let Some(radii) = radii {
        for (index, corner) in out.iter_mut().enumerate() {
            corner[0] = resolve(&radii.h[index], width).max(0.0);
            corner[1] = resolve(&radii.v[index], height).max(0.0);
        }
    }
    scale_radii(width, height, &mut out);
    out
}

struct Reference {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Reference {
    fn resolve_position(&self, at: &Option<Pos>) -> (f32, f32) {
        match at {
            None => (self.x + self.width / 2.0, self.y + self.height / 2.0),
            Some(pos) => {
                let (x, y) = pos.lens();
                (self.x + resolve(&x, self.width), self.y + resolve(&y, self.height))
            }
        }
    }
}

fn reference_box(name: &str, boxes: &FloatBox) -> Reference {
    let [mt, mr, mb, ml] = boxes.margin;
    let [bt, br, bb, bl] = boxes.border;
    let [pt, pr, pb, pl] = boxes.padding;
    let mut reference = Reference { x: 0.0, y: 0.0, width: boxes.width, height: boxes.height };
    let mut inset = |top: f32, right: f32, bottom: f32, left: f32| {
        reference.x += left;
        reference.y += top;
        reference.width = (reference.width - left - right).max(0.0);
        reference.height = (reference.height - top - bottom).max(0.0);
    };
    match name {
        "margin-box" => {}
        "border-box" => inset(mt, mr, mb, ml),
        "padding-box" => inset(mt + bt, mr + br, mb + bb, ml + bl),
        _ => inset(mt + bt + pt, mr + br + pr, mb + bb + pb, ml + bl + pl),
    }
    reference
}

fn box_radii(name: &str, style: &Style, boxes: &FloatBox, reference: &Reference) -> [[f32; 2]; 4] {
    let [mt, mr, mb, ml] = boxes.margin;
    let [bt, br, bb, bl] = boxes.border;
    let [pt, pr, pb, pl] = boxes.padding;
    let border_width = boxes.width - ml - mr;
    let border_height = boxes.height - mt - mb;
    let base = style.corner_radii(border_width.max(0.0), border_height.max(0.0));
    let sides = |offsets: [f32; 4], grow: bool| {
        let [top, right, bottom, left] = offsets;
        let delta = [[left, top], [right, top], [right, bottom], [left, bottom]];
        let mut out = base;
        for (corner, delta) in out.iter_mut().zip(delta) {
            for axis in 0..2 {
                if grow {
                    if corner[0] > 0.0 && corner[1] > 0.0 {
                        corner[axis] += delta[axis];
                    }
                } else {
                    corner[axis] = (corner[axis] - delta[axis]).max(0.0);
                }
            }
        }
        out
    };
    let mut radii = match name {
        "margin-box" => sides([mt, mr, mb, ml], true),
        "border-box" => base,
        "padding-box" => sides([bt, br, bb, bl], false),
        _ => sides([bt + pt, br + pr, bb + pb, bl + pl], false),
    };
    scale_radii(reference.width, reference.height, &mut radii);
    radii
}

fn closest_farthest(at: (f32, f32), reference: &Reference, closest: bool, corner: bool) -> f32 {
    let (cx, cy) = at;
    let left = (cx - reference.x).abs();
    let right = (reference.x + reference.width - cx).abs();
    let top = (cy - reference.y).abs();
    let bottom = (reference.y + reference.height - cy).abs();
    let pick = |values: &[f32]| {
        values.iter().copied().fold(if closest { f32::INFINITY } else { 0.0 }, |acc, v| if closest { acc.min(v) } else { acc.max(v) })
    };
    if corner {
        let distances = [
            libm::hypotf(left, top),
            libm::hypotf(right, top),
            libm::hypotf(left, bottom),
            libm::hypotf(right, bottom),
        ];
        pick(&distances)
    } else {
        pick(&[left, right, top, bottom])
    }
}

fn axis_extent(at: f32, start: f32, size: f32, closest: bool) -> f32 {
    let near = (at - start).abs();
    let far = (start + size - at).abs();
    if closest {
        near.min(far)
    } else {
        near.max(far)
    }
}

fn polygon_edges(points: &[(f32, f32)], edges: &mut Vec<[f32; 4]>) {
    for index in 0..points.len() {
        let from = points[index];
        let to = points[(index + 1) % points.len()];
        edges.push([from.0, from.1, to.0, to.1]);
    }
}

fn build_geometry(shape: &Shape, reference: &Reference) -> Option<Geometry> {
    Some(match shape {
        Shape::Inset { sides, radii } => {
            let top = resolve(&sides[0], reference.height);
            let right = resolve(&sides[1], reference.width);
            let bottom = resolve(&sides[2], reference.height);
            let left = resolve(&sides[3], reference.width);
            round_rect(
                reference.x + left,
                reference.y + top,
                reference.x + reference.width - right,
                reference.y + reference.height - bottom,
                radii,
            )
        }
        Shape::Rect { sides, radii } => {
            let top = sides[0].as_ref().map_or(0.0, |len| resolve(len, reference.height));
            let right = sides[1].as_ref().map_or(reference.width, |len| resolve(len, reference.width));
            let bottom = sides[2].as_ref().map_or(reference.height, |len| resolve(len, reference.height));
            let left = sides[3].as_ref().map_or(0.0, |len| resolve(len, reference.width));
            round_rect(reference.x + left, reference.y + top, reference.x + right.max(left), reference.y + bottom.max(top), radii)
        }
        Shape::Xywh { x, y, w, h, radii } => {
            let left = reference.x + resolve(x, reference.width);
            let top = reference.y + resolve(y, reference.height);
            round_rect(left, top, left + resolve(w, reference.width).max(0.0), top + resolve(h, reference.height).max(0.0), radii)
        }
        Shape::Circle { r, at } => {
            let center = reference.resolve_position(at);
            let radius = match r {
                Radius::Len(len) => {
                    let basis = libm::sqrtf((reference.width * reference.width + reference.height * reference.height) / 2.0);
                    resolve(len, basis)
                }
                Radius::ClosestSide => closest_farthest(center, reference, true, false),
                Radius::FarthestSide => closest_farthest(center, reference, false, false),
                Radius::ClosestCorner => closest_farthest(center, reference, true, true),
                Radius::FarthestCorner => closest_farthest(center, reference, false, true),
            }
            .max(0.0);
            Geometry::Ellipse { cx: center.0, cy: center.1, rx: radius, ry: radius }
        }
        Shape::Ellipse { rx, ry, at } => {
            let center = reference.resolve_position(at);
            let radius = |radius: &Radius, horizontal: bool| {
                match radius {
                    Radius::Len(len) => resolve(len, if horizontal { reference.width } else { reference.height }),
                    Radius::ClosestSide | Radius::ClosestCorner => {
                        if horizontal {
                            axis_extent(center.0, reference.x, reference.width, true)
                        } else {
                            axis_extent(center.1, reference.y, reference.height, true)
                        }
                    }
                    Radius::FarthestSide | Radius::FarthestCorner => {
                        if horizontal {
                            axis_extent(center.0, reference.x, reference.width, false)
                        } else {
                            axis_extent(center.1, reference.y, reference.height, false)
                        }
                    }
                }
                .max(0.0)
            };
            Geometry::Ellipse { cx: center.0, cy: center.1, rx: radius(rx, true), ry: radius(ry, false) }
        }
        Shape::Polygon { points, .. } => {
            let points: Vec<(f32, f32)> = points
                .iter()
                .map(|(x, y)| (reference.x + resolve(x, reference.width), reference.y + resolve(y, reference.height)))
                .collect();
            let mut edges = Vec::new();
            polygon_edges(&points, &mut edges);
            Geometry::Polygon { edges }
        }
        Shape::Path { data, .. } => {
            let parsed = lumen_common::svg_path::parse_svg_path(&data.absolute().text()).ok()?;
            let subpaths = lumen_common::svg_path_measure::flatten_subpaths(&parsed)?;
            let mut edges = Vec::new();
            for subpath in subpaths {
                let points: Vec<(f32, f32)> =
                    subpath.iter().map(|point| (reference.x + point[0], reference.y + point[1])).collect();
                if points.len() > 1 {
                    polygon_edges(&points, &mut edges);
                }
            }
            Geometry::Polygon { edges }
        }
        Shape::Function(_) | Shape::Ray { .. } => return None,
    })
}

fn round_rect(x0: f32, y0: f32, x1: f32, y1: f32, radii: &Option<Radii>) -> Geometry {
    let (x1, y1) = (x1.max(x0), y1.max(y0));
    Geometry::Round(RoundRect { x0, y0, x1, y1, radii: resolve_radii(radii, x1 - x0, y1 - y0) })
}

fn expand_round(rect: &mut RoundRect, margin: f32) {
    rect.x0 -= margin;
    rect.y0 -= margin;
    rect.x1 += margin;
    rect.y1 += margin;
    for corner in rect.radii.iter_mut() {
        corner[0] += margin;
        corner[1] += margin;
    }
}

fn corner_inset(radius: [f32; 2], depth: f32) -> f32 {
    if radius[1] <= 0.0 || radius[0] <= 0.0 || depth >= radius[1] {
        return 0.0;
    }
    let t = (radius[1] - depth) / radius[1];
    radius[0] * (1.0 - libm::sqrtf((1.0 - t * t).max(0.0)))
}

impl RoundRect {
    fn span(&self, y: f32) -> Option<(f32, f32)> {
        if y < self.y0 || y >= self.y1 {
            return None;
        }
        let from_top = y - self.y0;
        let from_bottom = self.y1 - y;
        let left = corner_inset(self.radii[0], from_top).max(corner_inset(self.radii[3], from_bottom));
        let right = corner_inset(self.radii[1], from_top).max(corner_inset(self.radii[2], from_bottom));
        let (lo, hi) = (self.x0 + left, self.x1 - right);
        (lo <= hi).then_some((lo, hi))
    }
}

fn polygon_span(edges: &[[f32; 4]], y: f32) -> Option<(f32, f32)> {
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for [x1, y1, x2, y2] in edges.iter().copied() {
        if y1 == y2 {
            continue;
        }
        let (top, bottom) = if y1 < y2 { (y1, y2) } else { (y2, y1) };
        if y < top || y >= bottom {
            continue;
        }
        let x = x1 + (y - y1) / (y2 - y1) * (x2 - x1);
        lo = lo.min(x);
        hi = hi.max(x);
    }
    (lo <= hi).then_some((lo, hi))
}

impl FloatShape {
    fn base_span(&self, y: f32) -> Option<(f32, f32)> {
        match &self.geometry {
            Geometry::Round(rect) => rect.span(y),
            Geometry::Ellipse { cx, cy, rx, ry } => {
                if *ry <= 0.0 || *rx <= 0.0 {
                    return None;
                }
                let t = (y - cy) / ry;
                if t.abs() >= 1.0 {
                    return None;
                }
                let half = rx * libm::sqrtf(1.0 - t * t);
                Some((cx - half, cx + half))
            }
            Geometry::Polygon { edges } => polygon_span(edges, y),
            Geometry::Raster(rows) => {
                let index = libm::floorf((y - rows.top) / rows.row_height);
                if index < 0.0 {
                    return None;
                }
                rows.rows.get(index as usize).copied().flatten()
            }
        }
    }

    fn needs_margin_sampling(&self) -> bool {
        self.margin > 0.0 && matches!(self.geometry, Geometry::Polygon { .. } | Geometry::Raster(_))
    }

    fn span(&self, y: f32) -> Option<(f32, f32)> {
        if y < 0.0 || y >= self.height {
            return None;
        }
        let (lo, hi) = if self.needs_margin_sampling() {
            let margin = self.margin;
            let steps = ((margin / SAMPLE_STEP).ceil() as usize).clamp(4, 128);
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for step in 0..=steps {
                let dy = -margin + 2.0 * margin * step as f32 / steps as f32;
                let reach = libm::sqrtf((margin * margin - dy * dy).max(0.0));
                if let Some((l, h)) = self.base_span(y + dy) {
                    lo = lo.min(l - reach);
                    hi = hi.max(h + reach);
                }
            }
            if lo > hi {
                return None;
            }
            (lo, hi)
        } else {
            self.base_span(y)?
        };
        Some((lo.clamp(0.0, self.width), hi.clamp(0.0, self.width)))
    }

    fn critical_rows(&self, out: &mut Vec<f32>) {
        match &self.geometry {
            Geometry::Round(rect) => {
                out.extend([rect.y0, rect.y1, rect.y0 + rect.radii[0][1], rect.y0 + rect.radii[1][1]]);
                out.extend([rect.y1 - rect.radii[2][1], rect.y1 - rect.radii[3][1]]);
            }
            Geometry::Ellipse { cy, ry, .. } => out.extend([*cy, cy - ry, cy + ry]),
            Geometry::Polygon { edges } => {
                for edge in edges {
                    out.push(edge[1]);
                    out.push(edge[3]);
                }
            }
            Geometry::Raster(rows) => {
                out.extend((0..=rows.rows.len()).map(|index| rows.top + index as f32 * rows.row_height));
            }
        }
    }

    /// The inline-axis edge the float area imposes on a line band, relative to
    /// the margin-box origin: the farthest right extent for a left float or
    /// the farthest left extent for a right float. `None` means the band does
    /// not meet the shape.
    pub(crate) fn band_edge(&self, left_float: bool, top: f32, bottom: f32) -> Option<f32> {
        let bottom = (bottom - 1.0 / LAYOUT_UNIT).max(top);
        let mut best: Option<f32> = None;
        let mut visit = |y: f32| {
            if let Some((lo, hi)) = self.span(y) {
                let value = if left_float { hi } else { lo };
                best = Some(match best {
                    None => value,
                    Some(current) if left_float => current.max(value),
                    Some(current) => current.min(value),
                });
            }
        };
        visit(top);
        visit(bottom);
        let mut rows = Vec::new();
        self.critical_rows(&mut rows);
        for row in rows {
            if row.is_finite() && row > top && row < bottom {
                visit(row);
                visit(row - 1.0 / LAYOUT_UNIT);
            }
        }
        if self.needs_margin_sampling() || matches!(self.geometry, Geometry::Raster(_)) {
            let count = (((bottom - top) / SAMPLE_STEP).ceil() as usize).clamp(1, MAX_SAMPLES);
            for step in 1..count {
                visit(top + (bottom - top) * step as f32 / count as f32);
            }
        }
        let edge = best?;
        Some(if left_float { libm::ceilf(edge * LAYOUT_UNIT) / LAYOUT_UNIT } else { libm::floorf(edge * LAYOUT_UNIT) / LAYOUT_UNIT })
    }
}

/// Whether `shape-outside` selects anything other than `none`.
pub(crate) fn applies(style: &Style) -> bool {
    extra_props::stored(style, "shape-outside").is_some_and(|stored| stored != "none")
}

fn shape_margin(style: &Style, containing_width: f32) -> f32 {
    let Some(stored) = extra_props::stored(style, "shape-margin") else { return 0.0 };
    let Some(Parsed::Len(len)) = parse(Prop::ShapeMargin, &stored, Env::new(static_length_context())) else { return 0.0 };
    resolve(&len, containing_width).max(0.0)
}

/// The shape-image-threshold, clamped to [0, 1].
pub(crate) fn image_threshold(style: &Style) -> f32 {
    let Some(stored) = extra_props::stored(style, "shape-image-threshold") else { return 0.0 };
    match parse(Prop::ShapeImageThreshold, &stored, Env::new(static_length_context())) {
        Some(Parsed::Number(value)) => value.clamp(0.0, 1.0) as f32,
        _ => 0.0,
    }
}

/// The computed `<image>` of `shape-outside` and its reference box, when the
/// property selects an image rather than a basic shape.
pub(crate) fn image_source(style: &Style) -> Option<(String, &'static str)> {
    let stored = extra_props::stored(style, "shape-outside")?;
    match parse(Prop::ShapeOutside, &stored, Env::new(static_length_context()))? {
        Parsed::Path(value) => match &value.source {
            Source::Image { computed, .. } => Some((computed.clone(), value.effective_box(Prop::ShapeOutside))),
            _ => None,
        },
        _ => None,
    }
}

/// The `url()` of a computed image value, if it is one.
pub(crate) fn image_url(computed: &str) -> Option<Arc<str>> {
    background_url(computed.trim())
}

/// Float-area rows of an RGBA bitmap stretched over the reference box
/// `(x, y, width, height)`: a row spans the pixels whose alpha exceeds the
/// threshold.
pub(crate) fn raster_rows(
    width: u32,
    height: u32,
    pixels: &[u8],
    threshold: f32,
    reference: (f32, f32, f32, f32),
) -> Option<RasterRows> {
    let (x, y, box_width, box_height) = reference;
    let (columns, rows) = (width as usize, height as usize);
    if columns == 0 || rows == 0 || pixels.len() != columns * rows * 4 || box_width <= 0.0 || box_height <= 0.0 {
        return None;
    }
    let limit = threshold * 255.0;
    let column_width = box_width / columns as f32;
    let mut spans = Vec::with_capacity(rows);
    for row in 0..rows {
        let line = &pixels[row * columns * 4..(row + 1) * columns * 4];
        let opaque = |column: usize| f32::from(line[column * 4 + 3]) > limit;
        let first = (0..columns).find(|column| opaque(*column));
        spans.push(first.map(|first| {
            let last = (0..columns).rev().find(|column| opaque(*column)).unwrap_or(first);
            (x + first as f32 * column_width, x + (last + 1) as f32 * column_width)
        }));
    }
    Some(RasterRows { top: y, row_height: box_height / rows as f32, rows: spans })
}

/// Resolves `shape-outside` for a float whose margin box has the given size.
/// Shapes taken from an image are produced by `raster` from the reference box
/// in margin-box coordinates as `(x, y, width, height)`.
pub(crate) fn float_shape(
    style: &Style,
    boxes: &FloatBox,
    containing_width: f32,
    raster: impl FnOnce(&str, (f32, f32, f32, f32)) -> Option<RasterRows>,
) -> Option<FloatShape> {
    let stored = extra_props::stored(style, "shape-outside")?;
    if stored == "none" {
        return None;
    }
    let Parsed::Path(value) = parse(Prop::ShapeOutside, &stored, Env::new(static_length_context()))? else { return None };
    let name = value.effective_box(Prop::ShapeOutside);
    let reference = reference_box(name, boxes);
    let margin = shape_margin(style, containing_width);
    let mut geometry = match &value.source {
        Source::None | Source::Url(_) => return None,
        Source::BoxOnly => {
            let radii = box_radii(name, style, boxes, &reference);
            Geometry::Round(RoundRect {
                x0: reference.x,
                y0: reference.y,
                x1: reference.x + reference.width,
                y1: reference.y + reference.height,
                radii,
            })
        }
        Source::Shape(shape) => build_geometry(shape, &reference)?,
        Source::Image { computed, .. } => {
            let rows = raster(computed, (reference.x, reference.y, reference.width, reference.height))?;
            Geometry::Raster(Arc::new(rows))
        }
    };
    match &mut geometry {
        Geometry::Round(rect) if margin > 0.0 => expand_round(rect, margin),
        Geometry::Ellipse { rx, ry, .. } if margin > 0.0 => {
            *rx += margin;
            *ry += margin;
        }
        _ => {}
    }
    Some(FloatShape { width: boxes.width, height: boxes.height, margin, geometry })
}
