//! The CSS 2.1 `clip` property: a `rect()` over the border box of an absolutely positioned box.
use super::*;
use crate::paint::Rect;

/// Computed `clip: rect(top, right, bottom, left)` offsets from the border box's top-left corner;
/// `None` entries are `auto` (the border-box edge). Only absolute and fixed boxes are clipped.
pub(crate) fn rect_offsets(style: &Style) -> Option<[Option<f32>; 4]> {
    if !matches!(style.position, Position::Absolute | Position::Fixed) {
        return None;
    }
    let text = extra_props::stored(style, "clip")?;
    let inner = text.trim().strip_prefix("rect(")?.strip_suffix(')')?;
    let mut sides = [None; 4];
    let mut count = 0;
    for part in inner.split(',') {
        let part = part.trim();
        let slot = sides.get_mut(count)?;
        if !part.eq_ignore_ascii_case("auto") {
            *slot = Some(part.strip_suffix("px")?.trim().parse::<f32>().ok().filter(|value| value.is_finite())?);
        }
        count += 1;
    }
    (count == 4).then_some(sides)
}

/// Intersect `clip` with the legacy clip rectangle of a border box.
pub(crate) fn apply(style: &Style, border_box: Rect, clip: Rect) -> Option<Rect> {
    let [top, right, bottom, left] = rect_offsets(style)?;
    let new_left = (border_box.x + left.unwrap_or(0.0)).max(clip.x);
    let new_top = (border_box.y + top.unwrap_or(0.0)).max(clip.y);
    let new_right = (border_box.x + right.unwrap_or(border_box.width)).min(clip.x + clip.width);
    let new_bottom = (border_box.y + bottom.unwrap_or(border_box.height)).min(clip.y + clip.height);
    Some(Rect {
        x: new_left,
        y: new_top,
        width: (new_right - new_left).max(0.0),
        height: (new_bottom - new_top).max(0.0),
    })
}
