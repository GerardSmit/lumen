//! Path geometry for the 2D canvas: roundRect(), arcTo() and hit testing.
use super::*;
use tiny_skia::{PathSegment, Point as SkPoint, Stroke};

const KAPPA: f64 = 0.552_284_749_830_793_4;
const FLATTEN_STEPS: usize = 32;
const EDGE_EPSILON: f64 = 1e-6;

pub(super) type Corner = (f64, f64);

fn iterator_method(ctx: &mut Ctx, value: &Value) -> OpResult<Option<Value>> {
    if !matches!(value, Value::Obj(_)) {
        return Ok(None);
    }
    let key = ctx
        .well_known_symbol("iterator")
        .ok_or_else(|| OpError::type_error("Symbol.iterator is unavailable"))?;
    let method = ctx
        .reflect_get(value, &key, value)
        .map_err(OpError::thrown)?;
    Ok((!matches!(method, Value::Undefined | Value::Null)).then_some(method))
}

fn optional_member(ctx: &mut Ctx, value: &Value, name: &str) -> OpResult<f64> {
    let member = ctx.member_get(value, name).map_err(OpError::thrown)?;
    if matches!(member, Value::Undefined) {
        Ok(0.0)
    } else {
        ctx.coerce_number(&member).map_err(OpError::thrown)
    }
}

fn corner_radius(ctx: &mut Ctx, value: &Value) -> OpResult<Corner> {
    match value {
        Value::Undefined | Value::Null => Ok((0.0, 0.0)),
        Value::Obj(_) => {
            let x = optional_member(ctx, value, "x")?;
            let y = optional_member(ctx, value, "y")?;
            Ok((x, y))
        }
        other => {
            let radius = ctx.coerce_number(other).map_err(OpError::thrown)?;
            Ok((radius, radius))
        }
    }
}

pub(super) fn parse_radii(ctx: &mut Ctx, radii: Option<Value>) -> OpResult<Vec<Corner>> {
    let Some(radii) = radii else {
        return Ok(vec![(0.0, 0.0)]);
    };
    if let Some(method) = iterator_method(ctx, &radii)? {
        let items = ctx.convert_iterable_with_method(&radii, method, 4, |ctx, value| {
            corner_radius(ctx, &value)
        })?;
        if items.is_empty() {
            return Err(OpError::range_error("roundRect radii must have 1 to 4 items"));
        }
        return Ok(items);
    }
    Ok(vec![corner_radius(ctx, &radii)?])
}

/// Upper-left, upper-right, lower-right, lower-left per the CSS `border-radius` expansion.
fn expand_radii(radii: &[Corner]) -> [Corner; 4] {
    match *radii {
        [all] => [all; 4],
        [a, b] => [a, b, a, b],
        [a, b, c] => [a, b, c, b],
        [a, b, c, d] => [a, b, c, d],
        _ => [(0.0, 0.0); 4],
    }
}

impl CanvasPath {
    fn current_point(&self) -> Option<(f64, f64)> {
        let parsed = self.build_parsed(Transform::identity());
        parsed
            .has_subpath
            .then_some((f64::from(parsed.current.0), f64::from(parsed.current.1)))
    }

    pub(super) fn round_rect(
        &mut self,
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        radii: &[Corner],
    ) -> OpResult<()> {
        if ![x, y, width, height].iter().all(|v| v.is_finite()) {
            return Ok(());
        }
        if radii.iter().any(|(rx, ry)| !rx.is_finite() || !ry.is_finite()) {
            return Ok(());
        }
        if radii.iter().any(|(rx, ry)| *rx < 0.0 || *ry < 0.0) {
            return Err(OpError::range_error("roundRect radii must not be negative"));
        }
        self.push_round_rect(x, y, width, height, expand_radii(radii));
        Ok(())
    }

    fn push_round_rect(&mut self, x: f64, y: f64, w: f64, h: f64, mut radii: [Corner; 4]) {
        let [ul, ur, lr, ll] = radii;
        let mut scale = 1.0f64;
        for (length, total) in [
            (w.abs(), ul.0 + ur.0),
            (h.abs(), ur.1 + lr.1),
            (w.abs(), lr.0 + ll.0),
            (h.abs(), ul.1 + ll.1),
        ] {
            if total > 0.0 {
                scale = scale.min(length / total);
            }
        }
        if scale < 1.0 {
            for radius in &mut radii {
                radius.0 *= scale;
                radius.1 *= scale;
            }
        }
        let [ul, ur, lr, ll] = radii;
        let (sx, sy) = (
            if w < 0.0 { -1.0 } else { 1.0 },
            if h < 0.0 { -1.0 } else { 1.0 },
        );
        let k = 1.0 - KAPPA;
        let line = |commands: &mut Vec<PathCommand>, px: f64, py: f64| {
            commands.push(PathCommand::Line(px as f32, py as f32));
        };
        let curve = |commands: &mut Vec<PathCommand>,
                     radius: Corner,
                     c1: (f64, f64),
                     c2: (f64, f64),
                     end: (f64, f64)| {
            if radius.0 == 0.0 && radius.1 == 0.0 {
                commands.push(PathCommand::Line(end.0 as f32, end.1 as f32));
            } else {
                commands.push(PathCommand::Cubic(
                    c1.0 as f32,
                    c1.1 as f32,
                    c2.0 as f32,
                    c2.1 as f32,
                    end.0 as f32,
                    end.1 as f32,
                ));
            }
        };
        let commands = &mut self.commands;
        commands.push(PathCommand::Move((x + sx * ul.0) as f32, y as f32));
        line(commands, x + w - sx * ur.0, y);
        curve(
            commands,
            ur,
            (x + w - sx * ur.0 * k, y),
            (x + w, y + sy * ur.1 * k),
            (x + w, y + sy * ur.1),
        );
        line(commands, x + w, y + h - sy * lr.1);
        curve(
            commands,
            lr,
            (x + w, y + h - sy * lr.1 * k),
            (x + w - sx * lr.0 * k, y + h),
            (x + w - sx * lr.0, y + h),
        );
        line(commands, x + sx * ll.0, y + h);
        curve(
            commands,
            ll,
            (x + sx * ll.0 * k, y + h),
            (x, y + h - sy * ll.1 * k),
            (x, y + h - sy * ll.1),
        );
        line(commands, x, y + sy * ul.1);
        curve(
            commands,
            ul,
            (x, y + sy * ul.1 * k),
            (x + sx * ul.0 * k, y),
            (x + sx * ul.0, y),
        );
        commands.push(PathCommand::Close);
        commands.push(PathCommand::Move(x as f32, y as f32));
    }

    pub(super) fn arc_to(
        &mut self,
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        radius: f64,
    ) -> OpResult<()> {
        if ![x1, y1, x2, y2, radius].iter().all(|v| v.is_finite()) {
            return Ok(());
        }
        if radius < 0.0 {
            return Err(OpError::new(
                "IndexSizeError",
                "arcTo radius must be non-negative",
            ));
        }
        let line_to_p1 = |path: &mut Self| {
            path.commands.push(PathCommand::Line(x1 as f32, y1 as f32));
        };
        let Some((x0, y0)) = self.current_point() else {
            self.commands.push(PathCommand::Move(x1 as f32, y1 as f32));
            line_to_p1(self);
            return Ok(());
        };
        let (d0x, d0y) = (x0 - x1, y0 - y1);
        let (d2x, d2y) = (x2 - x1, y2 - y1);
        let (l0, l2) = (d0x.hypot(d0y), d2x.hypot(d2y));
        if l0 == 0.0 || l2 == 0.0 || radius == 0.0 {
            line_to_p1(self);
            return Ok(());
        }
        let cross = d0x * d2y - d0y * d2x;
        if cross.abs() <= 1e-12 * l0 * l2 {
            line_to_p1(self);
            return Ok(());
        }
        let (u0x, u0y) = (d0x / l0, d0y / l0);
        let (u2x, u2y) = (d2x / l2, d2y / l2);
        let half = (u0x * u2x + u0y * u2y).clamp(-1.0, 1.0).acos() / 2.0;
        let tangent_distance = radius / half.tan();
        let (bx, by) = (u0x + u2x, u0y + u2y);
        let bisector = bx.hypot(by);
        if bisector == 0.0 || !tangent_distance.is_finite() {
            line_to_p1(self);
            return Ok(());
        }
        let center_distance = radius / half.sin();
        let (cx, cy) = (
            x1 + bx / bisector * center_distance,
            y1 + by / bisector * center_distance,
        );
        let t1 = (x1 + u0x * tangent_distance, y1 + u0y * tangent_distance);
        let t2 = (x1 + u2x * tangent_distance, y1 + u2y * tangent_distance);
        let start = (t1.1 - cy).atan2(t1.0 - cx);
        let end = (t2.1 - cy).atan2(t2.0 - cx);
        let mut delta = end - start;
        while delta > std::f64::consts::PI {
            delta -= 2.0 * std::f64::consts::PI;
        }
        while delta <= -std::f64::consts::PI {
            delta += 2.0 * std::f64::consts::PI;
        }
        self.commands.push(PathCommand::Arc {
            cx: cx as f32,
            cy: cy as f32,
            rx: radius as f32,
            ry: radius as f32,
            rotation: 0.0,
            start: start as f32,
            end: (start + delta) as f32,
            counterclockwise: delta < 0.0,
        });
        Ok(())
    }
}

fn flatten(path: &Path) -> Vec<Vec<(f64, f64)>> {
    let mut subpaths: Vec<Vec<(f64, f64)>> = Vec::new();
    let mut current: Vec<(f64, f64)> = Vec::new();
    let mut last = (0.0f64, 0.0f64);
    let point = |p: SkPoint| (f64::from(p.x), f64::from(p.y));
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(p) => {
                if current.len() > 1 {
                    subpaths.push(std::mem::take(&mut current));
                }
                current.clear();
                last = point(p);
                current.push(last);
            }
            PathSegment::LineTo(p) => {
                last = point(p);
                current.push(last);
            }
            PathSegment::QuadTo(c, p) => {
                let (c, p) = (point(c), point(p));
                for step in 1..=FLATTEN_STEPS {
                    let t = step as f64 / FLATTEN_STEPS as f64;
                    let u = 1.0 - t;
                    current.push((
                        u * u * last.0 + 2.0 * u * t * c.0 + t * t * p.0,
                        u * u * last.1 + 2.0 * u * t * c.1 + t * t * p.1,
                    ));
                }
                last = p;
            }
            PathSegment::CubicTo(c1, c2, p) => {
                let (c1, c2, p) = (point(c1), point(c2), point(p));
                for step in 1..=FLATTEN_STEPS {
                    let t = step as f64 / FLATTEN_STEPS as f64;
                    let u = 1.0 - t;
                    current.push((
                        u * u * u * last.0
                            + 3.0 * u * u * t * c1.0
                            + 3.0 * u * t * t * c2.0
                            + t * t * t * p.0,
                        u * u * u * last.1
                            + 3.0 * u * u * t * c1.1
                            + 3.0 * u * t * t * c2.1
                            + t * t * t * p.1,
                    ));
                }
                last = p;
            }
            PathSegment::Close => {
                if let Some(first) = current.first().copied() {
                    current.push(first);
                    last = first;
                }
            }
        }
    }
    if current.len() > 1 {
        subpaths.push(current);
    }
    subpaths
}

fn on_segment(a: (f64, f64), b: (f64, f64), p: (f64, f64)) -> bool {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length_squared = dx * dx + dy * dy;
    let t = if length_squared == 0.0 {
        0.0
    } else {
        (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / length_squared).clamp(0.0, 1.0)
    };
    let (nx, ny) = (a.0 + t * dx - p.0, a.1 + t * dy - p.1);
    nx * nx + ny * ny <= EDGE_EPSILON * EDGE_EPSILON
}

/// Whether the device point lies in or on the boundary of the filled path.
pub(super) fn fill_contains(path: &Path, x: f64, y: f64, rule: FillRule) -> bool {
    let target = (x, y);
    let mut winding = 0i32;
    let mut crossings = 0i32;
    for subpath in flatten(path) {
        let count = subpath.len();
        for index in 0..count {
            let a = subpath[index];
            let b = subpath[(index + 1) % count];
            if on_segment(a, b, target) {
                return true;
            }
            if (a.1 <= y) != (b.1 <= y) {
                let side = (b.0 - a.0) * (y - a.1) - (x - a.0) * (b.1 - a.1);
                if (b.1 > a.1 && side > 0.0) || (b.1 < a.1 && side < 0.0) {
                    winding += if b.1 > a.1 { 1 } else { -1 };
                    crossings += 1;
                }
            }
        }
    }
    match rule {
        FillRule::Winding => winding != 0,
        FillRule::EvenOdd => crossings % 2 != 0,
    }
}

pub(super) fn stroke_contains(path: &Path, stroke: &Stroke, x: f64, y: f64) -> bool {
    let Some(outline) = path.stroke(stroke, 1.0) else {
        return false;
    };
    fill_contains(&outline, x, y, FillRule::Winding)
}

impl CanvasPath {
    /// Appends a command given in user space to a path stored in device space.
    pub(super) fn push_mapped(&mut self, ctm: Transform, command: PathCommand) {
        let map = |x: f32, y: f32| {
            let mut point = SkPoint::from_xy(x, y);
            ctm.map_point(&mut point);
            (point.x, point.y)
        };
        let mapped = match command {
            PathCommand::Move(x, y) => {
                let (x, y) = map(x, y);
                PathCommand::Move(x, y)
            }
            PathCommand::Line(x, y) => {
                let (x, y) = map(x, y);
                PathCommand::Line(x, y)
            }
            PathCommand::Quad(x1, y1, x, y) => {
                let (x1, y1) = map(x1, y1);
                let (x, y) = map(x, y);
                PathCommand::Quad(x1, y1, x, y)
            }
            PathCommand::Cubic(x1, y1, x2, y2, x, y) => {
                let (x1, y1) = map(x1, y1);
                let (x2, y2) = map(x2, y2);
                let (x, y) = map(x, y);
                PathCommand::Cubic(x1, y1, x2, y2, x, y)
            }
            other => other,
        };
        self.commands.push(mapped);
    }

    /// Runs `build` against a user-space copy of the current subpath and appends the
    /// result, transformed by `ctm`, to this device-space path.
    pub(super) fn append_in_user_space(
        &mut self,
        ctm: Transform,
        build: impl FnOnce(&mut CanvasPath) -> OpResult<()>,
    ) -> OpResult<()> {
        if ctm.is_identity() {
            return build(self);
        }
        let Some(inverse) = ctm.invert() else {
            return Ok(());
        };
        let start = self.current_point().map(|(x, y)| {
            let mut point = SkPoint::from_xy(x as f32, y as f32);
            inverse.map_point(&mut point);
            point
        });
        let mut scratch = CanvasPath::default();
        if let Some(point) = start {
            scratch.commands.push(PathCommand::Move(point.x, point.y));
        }
        build(&mut scratch)?;
        let Some(path) = scratch.build(ctm) else {
            return Ok(());
        };
        let mut skip_move = start.is_some();
        for segment in path.segments() {
            let command = match segment {
                PathSegment::MoveTo(point) => {
                    if std::mem::take(&mut skip_move) {
                        continue;
                    }
                    PathCommand::Move(point.x, point.y)
                }
                PathSegment::LineTo(point) => PathCommand::Line(point.x, point.y),
                PathSegment::QuadTo(c, point) => PathCommand::Quad(c.x, c.y, point.x, point.y),
                PathSegment::CubicTo(c1, c2, point) => {
                    PathCommand::Cubic(c1.x, c1.y, c2.x, c2.y, point.x, point.y)
                }
                PathSegment::Close => PathCommand::Close,
            };
            skip_move = false;
            self.commands.push(command);
        }
        Ok(())
    }
}

pub(super) fn all_finite(values: &[f64]) -> bool {
    values.iter().all(|value| value.is_finite())
}
