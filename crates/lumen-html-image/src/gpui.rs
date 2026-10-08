//! GPUI window.rs paint_quad device geometry. Keep ties toward zero,
//! nonzero strokes at least one device pixel, and content masks covering
//! their logical bounds (gpui/src/util.rs and window.rs).
use lumen_html::paint::{Command, DisplayList, Rect};

/// Uniform-radius, uniform-border signed-distance coverage of a native GPUI
/// quad. The geometry follows GPUI's quad fragment shader; unlike the image
/// path's contour-area integration, native quads use a one-pixel SDF ramp.
pub(crate) fn draw_quad(
    sink: &mut super::Raster<'_>,
    rect: Rect,
    radius: f32,
    border: Option<f32>,
    color: lumen_html::paint::Rgba,
) {
    if color.a == 0 || border == Some(0.0) {
        return;
    }
    let Some(visible) = sink.clips.last().and_then(|clip| clip.intersection(rect)) else {
        return;
    };
    let (x0, y0, x1, y1) = sink.pixel_span(visible);
    let half = [
        rect.width * sink.scale * 0.5,
        rect.height * sink.scale * 0.5,
    ];
    let center = [rect.x * sink.scale + half[0], rect.y * sink.scale + half[1]];
    let r = radius * sink.scale;
    for y in y0..y1 {
        for x in x0..x1 {
            let corner = [
                (x as f32 + 0.5 - center[0]).abs() - half[0],
                (y as f32 + 0.5 - center[1]).abs() - half[1],
            ];
            let q = [corner[0] + r, corner[1] + r];
            let outer = if r == 0.0 {
                q[0].max(q[1])
            } else {
                q[0].max(0.0).hypot(q[1].max(0.0)) + q[0].max(q[1]).min(0.0) - r
            };
            let mut coverage = (0.5 - outer).clamp(0.0, 1.0);
            let mut rgb_scale = 1.0;
            if let Some(width) = border {
                let w = width * sink.scale;
                let straight = [corner[0] + w, corner[1] + w];
                let inner = if q[0] <= 0.0 || q[1] <= 0.0 {
                    -straight[0].max(straight[1])
                } else if straight[0] > 0.0 || straight[1] > 0.0 {
                    -1.0
                } else {
                    -(outer + w)
                };
                let ramp = (0.5 - inner).clamp(0.0, 1.0);
                coverage *= ramp;
                // GPUI interpolates straight RGB and alpha independently
                // between transparent black and the border shader color.
                rgb_scale = ramp;
            }
            if coverage > 0.0 {
                let offset = (y as usize * sink.image.width as usize + x as usize) * 4;
                super::composite_native(
                    &mut sink.image.pixels[offset..offset + 4],
                    color,
                    coverage,
                    rgb_scale,
                );
            }
        }
    }
}

fn nearest(value: f32, scale: f32) -> f32 {
    let device = value * scale;
    (device.abs() - 0.5).ceil().copysign(device) / scale
}

fn quad(rect: &mut Rect, scale: f32) {
    let right = nearest(rect.x + rect.width, scale);
    let bottom = nearest(rect.y + rect.height, scale);
    rect.x = nearest(rect.x, scale);
    rect.y = nearest(rect.y, scale);
    rect.width = (right - rect.x).max(0.0);
    rect.height = (bottom - rect.y).max(0.0);
}

pub(crate) fn snap(list: &mut DisplayList, scale: f32) {
    let mut transforms = 0usize;
    for command in &mut list.0 {
        match command {
            Command::PushTransform(_) => transforms += 1,
            Command::PopTransform => transforms = transforms.saturating_sub(1),
            Command::PushBoxClip(rect) if transforms == 0 => quad(rect, scale),
            Command::FillRect { rect, .. } | Command::FillRoundedRect { rect, .. } => {
                quad(rect, scale)
            }
            Command::StrokeBorder { rect, width, .. } => {
                quad(rect, scale);
                if *width != 0.0 {
                    *width = nearest(width.max(0.0), scale).max(1.0 / scale);
                }
            }
            Command::PushClip(rect) => {
                let right = ((rect.x + rect.width) * scale).ceil() / scale;
                let bottom = ((rect.y + rect.height) * scale).ceil() / scale;
                rect.x = (rect.x * scale).floor() / scale;
                rect.y = (rect.y * scale).floor() / scale;
                rect.width = (right - rect.x).max(0.0);
                rect.height = (bottom - rect.y).max(0.0);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn device_edges_ties_thin_strokes_and_cover_masks_match_gpui() {
        assert_eq!(nearest(0.25, 2.0), 0.0);
        assert_eq!(nearest(-0.25, 2.0), 0.0);
        assert_eq!(nearest(0.26, 2.0), 0.5);
        assert_eq!(nearest(-0.26, 2.0), -0.5);
        let rect = Rect {
            x: -0.25,
            y: 0.25,
            width: 2.0,
            height: 2.0,
        };
        let mut list = DisplayList(vec![
            Command::StrokeBorder {
                rect,
                radius: 0.5,
                width: 0.01,
                color: lumen_html::paint::Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 255,
                },
            },
            Command::PushClip(rect),
            Command::PopClip,
        ]);
        snap(&mut list, 2.0);
        assert!(matches!(
            list.0[0],
            Command::StrokeBorder {
                rect: Rect {
                    x: 0.0,
                    y: 0.0,
                    width: 1.5,
                    height: 2.0
                },
                width: 0.5,
                radius: 0.5,
                ..
            }
        ));
        assert!(matches!(
            list.0[1],
            Command::PushClip(Rect {
                x: -0.5,
                y: 0.0,
                width: 2.5,
                height: 2.5
            })
        ));
    }
}
