//! Device edge snapping for untransformed CSS rectangular box painting.
//! Geometry and hit testing retain CSS coordinates. Transformed descendants
//! retain antialiased geometry; replay applies their transform afterwards.
use lumen_html::paint::{Command, DisplayList, Rect};

fn rectangle(rect: &mut Rect, scale: f32) {
    let edge = |value: f32| (value * scale + 0.5).floor() / scale;
    let right = edge(rect.x + rect.width);
    let bottom = edge(rect.y + rect.height);
    rect.x = edge(rect.x);
    rect.y = edge(rect.y);
    rect.width = (right - rect.x).max(0.0);
    rect.height = (bottom - rect.y).max(0.0);
}

pub(crate) fn boxes(list: &mut DisplayList, scale: f32) {
    let mut transforms = 0usize;
    for command in &mut list.0 {
        match command {
            Command::PushTransform(_) => transforms += 1,
            Command::PopTransform => transforms = transforms.saturating_sub(1),
            Command::FillRect { rect, .. } if transforms == 0 => rectangle(rect, scale),
            Command::StrokeBorder {
                rect,
                radius,
                width,
                ..
            } if transforms == 0 && *radius == 0.0 => {
                rectangle(rect, scale);
                if *width > 0.0 {
                    *width = (*width * scale).floor().max(1.0) / scale;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_html::paint::{Affine, Rgba};
    #[test]
    fn css_boxes_snap_edges_but_transformed_children_keep_fractional_geometry() {
        let rect = Rect {
            x: 11.5,
            y: 4.25,
            width: 30.0,
            height: 20.0,
        };
        let color = Rgba {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        };
        let mut list = DisplayList(vec![
            Command::FillRect { rect, color },
            Command::PushTransform(Affine::IDENTITY),
            Command::FillRect { rect, color },
            Command::PopTransform,
        ]);
        boxes(&mut list, 1.0);
        assert!(matches!(
            list.0[0],
            Command::FillRect {
                rect: Rect {
                    x: 12.0,
                    y: 4.0,
                    width: 30.0,
                    height: 20.0
                },
                ..
            }
        ));
        assert!(matches!(list.0[2], Command::FillRect { rect: original, .. } if original == rect));
    }
}
