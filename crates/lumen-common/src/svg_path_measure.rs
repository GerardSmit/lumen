//! Arc-length measurement of parsed SVG paths, used to lay text out along a path.
use crate::svg_path::ParsedSvgPath;
use alloc::vec::Vec;
use tiny_skia_path::PathSegment;

const MAX_POINTS: usize = 65_536;
const MAX_CURVE_STEPS: usize = 256;
const CURVE_STEP_LENGTH: f32 = 1.5;

/// A path flattened to a polyline with cumulative arc lengths.
pub struct MeasuredPath {
    points: Vec<[f32; 2]>,
    ends: Vec<f32>,
}

/// A point on a measured path and the unit tangent of the path there.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathPoint {
    pub x: f32,
    pub y: f32,
    pub tangent: [f32; 2],
}

impl MeasuredPath {
    pub fn new(parsed: &ParsedSvgPath) -> Option<Self> {
        let path = parsed.path.as_ref()?;
        let mut measured = Self { points: Vec::new(), ends: Vec::new() };
        let mut start = [0.0f32, 0.0];
        for segment in path.segments() {
            match segment {
                PathSegment::MoveTo(point) => {
                    start = [point.x, point.y];
                    measured.jump(start)?;
                }
                PathSegment::LineTo(point) => measured.line([point.x, point.y])?,
                PathSegment::QuadTo(control, point) => {
                    let from = measured.points.last().copied()?;
                    let control = [control.x, control.y];
                    let to = [point.x, point.y];
                    let steps = curve_steps(&[from, control, to]);
                    for step in 1..=steps {
                        let t = step as f32 / steps as f32;
                        let u = 1.0 - t;
                        measured.line([
                            u * u * from[0] + 2.0 * u * t * control[0] + t * t * to[0],
                            u * u * from[1] + 2.0 * u * t * control[1] + t * t * to[1],
                        ])?;
                    }
                }
                PathSegment::CubicTo(first, second, point) => {
                    let from = measured.points.last().copied()?;
                    let first = [first.x, first.y];
                    let second = [second.x, second.y];
                    let to = [point.x, point.y];
                    let steps = curve_steps(&[from, first, second, to]);
                    for step in 1..=steps {
                        let t = step as f32 / steps as f32;
                        let u = 1.0 - t;
                        let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                        measured.line([
                            a * from[0] + b * first[0] + c * second[0] + d * to[0],
                            a * from[1] + b * first[1] + c * second[1] + d * to[1],
                        ])?;
                    }
                }
                PathSegment::Close => measured.line(start)?,
            }
        }
        (measured.length() > 0.0).then_some(measured)
    }

    pub fn length(&self) -> f32 {
        self.ends.last().copied().unwrap_or(0.0)
    }

    /// The point at `distance` along the path, or `None` outside `[0, length]`.
    pub fn at(&self, distance: f32) -> Option<PathPoint> {
        if !distance.is_finite() || distance < 0.0 || distance > self.length() {
            return None;
        }
        let mut index = self.ends.partition_point(|end| *end < distance).max(1);
        if index < self.ends.len() && self.ends[index] == self.ends[index - 1] {
            index = self.ends.partition_point(|end| *end <= self.ends[index]);
        }
        let (from, to) = (*self.points.get(index - 1)?, *self.points.get(index)?);
        let span = self.ends[index] - self.ends[index - 1];
        let t = ((distance - self.ends[index - 1]) / span).clamp(0.0, 1.0);
        let tangent = [(to[0] - from[0]) / span, (to[1] - from[1]) / span];
        Some(PathPoint {
            x: from[0] + (to[0] - from[0]) * t,
            y: from[1] + (to[1] - from[1]) * t,
            tangent,
        })
    }

    fn jump(&mut self, to: [f32; 2]) -> Option<()> {
        let end = self.length();
        self.push(to, end)
    }

    fn line(&mut self, to: [f32; 2]) -> Option<()> {
        let from = self.points.last().copied()?;
        let length = libm::hypotf(to[0] - from[0], to[1] - from[1]);
        let end = self.length() + length;
        self.push(to, end)
    }

    fn push(&mut self, point: [f32; 2], end: f32) -> Option<()> {
        if self.points.len() >= MAX_POINTS || !point[0].is_finite() || !point[1].is_finite() {
            return None;
        }
        self.points.push(point);
        self.ends.push(end);
        Some(())
    }
}

/// Flattens every subpath into a polyline; closed subpaths end at their start.
pub fn flatten_subpaths(parsed: &ParsedSvgPath) -> Option<Vec<Vec<[f32; 2]>>> {
    let path = parsed.path.as_ref()?;
    let mut subpaths: Vec<Vec<[f32; 2]>> = Vec::new();
    let mut start = [0.0f32, 0.0];
    for segment in path.segments() {
        match segment {
            PathSegment::MoveTo(point) => {
                start = [point.x, point.y];
                subpaths.push(vec![start]);
            }
            PathSegment::LineTo(point) => subpaths.last_mut()?.push([point.x, point.y]),
            PathSegment::QuadTo(control, point) => {
                let current = subpaths.last_mut()?;
                let from = *current.last()?;
                let control = [control.x, control.y];
                let to = [point.x, point.y];
                let steps = curve_steps(&[from, control, to]);
                for step in 1..=steps {
                    let t = step as f32 / steps as f32;
                    let u = 1.0 - t;
                    current.push([
                        u * u * from[0] + 2.0 * u * t * control[0] + t * t * to[0],
                        u * u * from[1] + 2.0 * u * t * control[1] + t * t * to[1],
                    ]);
                }
            }
            PathSegment::CubicTo(first, second, point) => {
                let current = subpaths.last_mut()?;
                let from = *current.last()?;
                let first = [first.x, first.y];
                let second = [second.x, second.y];
                let to = [point.x, point.y];
                let steps = curve_steps(&[from, first, second, to]);
                for step in 1..=steps {
                    let t = step as f32 / steps as f32;
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    current.push([
                        a * from[0] + b * first[0] + c * second[0] + d * to[0],
                        a * from[1] + b * first[1] + c * second[1] + d * to[1],
                    ]);
                }
            }
            PathSegment::Close => {
                let current = subpaths.last_mut()?;
                current.push(start);
            }
        }
    }
    Some(subpaths)
}

fn curve_steps(control_polygon: &[[f32; 2]]) -> usize {
    let length: f32 = control_polygon
        .windows(2)
        .map(|pair| libm::hypotf(pair[1][0] - pair[0][0], pair[1][1] - pair[0][1]))
        .sum();
    if !length.is_finite() {
        return 1;
    }
    ((length / CURVE_STEP_LENGTH).ceil() as usize).clamp(2, MAX_CURVE_STEPS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::svg_path::parse_svg_path;

    #[test]
    fn measures_lines_and_circles() {
        let line = MeasuredPath::new(&parse_svg_path("M0,0 L30,40").unwrap()).unwrap();
        assert!((line.length() - 50.0).abs() < 1e-3);
        let middle = line.at(25.0).unwrap();
        assert!((middle.x - 15.0).abs() < 1e-3 && (middle.y - 20.0).abs() < 1e-3);
        assert!((middle.tangent[0] - 0.6).abs() < 1e-3 && (middle.tangent[1] - 0.8).abs() < 1e-3);
        assert!(line.at(50.5).is_none());

        let circle = MeasuredPath::new(
            &parse_svg_path("M100,200 m-100,0 a100,100 0 1,1 200,0 a100,100 0 1,1 -200,0").unwrap(),
        )
        .unwrap();
        assert!((circle.length() - 628.3).abs() < 2.0);
        let top = circle.at(circle.length() / 4.0).unwrap();
        assert!((top.x - 100.0).abs() < 1.0 && (top.y - 100.0).abs() < 1.0);
        assert!(top.tangent[0] > 0.99);
    }
}
