//! `object-fit` and `object-position`: where a replaced element's image lands inside its
//! content box (CSS Images 3 section 5).
use super::*;
use crate::object::IntrinsicSize;
use crate::paint::Rect;

const DEFAULT_OBJECT_SIZE: (f32, f32) = (300.0, 150.0);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Base {
    Contain,
    Cover,
    None,
}

pub(crate) struct ObjectFit {
    base: Base,
    scale_down: bool,
    position: [TransformLength; 2],
}

impl ObjectFit {
    /// The element's fit, or `None` for `fill`, which stretches the image over the content box.
    pub(crate) fn of(style: &Style) -> Option<Self> {
        let specified = extra_props::stored(style, "object-fit")?;
        let mut base = None;
        let mut scale_down = false;
        for token in components(&specified)? {
            if decoded_css_keyword(token, "contain") {
                base = Some(Base::Contain);
            } else if decoded_css_keyword(token, "cover") {
                base = Some(Base::Cover);
            } else if decoded_css_keyword(token, "none") {
                base = Some(Base::None);
            } else if decoded_css_keyword(token, "scale-down") {
                scale_down = true;
            }
        }
        let base = match (base, scale_down) {
            (Some(base), _) => base,
            (None, true) => Base::Contain,
            (None, false) => return None,
        };
        Some(Self { base, scale_down, position: position(style) })
    }

    /// The rectangle the image is drawn into; it may extend beyond `content`, which then clips it.
    pub(crate) fn destination(&self, content: Rect, natural: IntrinsicSize) -> Rect {
        let content_size = (content.width, content.height);
        let size = match (self.base, self.scale_down) {
            (Base::None, _) => none_size(natural),
            (base, false) => constrained(base, content_size, natural),
            (base, true) => {
                let none = none_size(natural);
                let constrained = constrained(base, content_size, natural);
                if none.0 <= constrained.0 && none.1 <= constrained.1 { none } else { constrained }
            }
        };
        Rect {
            x: content.x + self.position[0].resolve(content.width - size.0),
            y: content.y + self.position[1].resolve(content.height - size.1),
            width: size.0,
            height: size.1,
        }
    }
}

fn position(style: &Style) -> [TransformLength; 2] {
    let center = TransformLength { pixels: 0.0, percent: 50.0 };
    extra_props::computed(style, "object-position")
        .and_then(|computed| background_position(&components(&computed)?, static_length_context()))
        .map_or([center, center], |parsed| parsed.map(TransformLength::from_used))
}

fn ratio_of(natural: IntrinsicSize) -> Option<f32> {
    natural.ratio.filter(|ratio| ratio.is_finite() && *ratio > 0.0).or_else(|| match (natural.width, natural.height) {
        (Some(width), Some(height)) if width > 0.0 && height > 0.0 => Some(width / height),
        _ => None,
    })
}

fn contain_within(bounds: (f32, f32), ratio: f32) -> (f32, f32) {
    if bounds.0 >= bounds.1 * ratio { (bounds.1 * ratio, bounds.1) } else { (bounds.0, bounds.0 / ratio) }
}

fn cover_over(bounds: (f32, f32), ratio: f32) -> (f32, f32) {
    if bounds.0 >= bounds.1 * ratio { (bounds.0, bounds.0 / ratio) } else { (bounds.1 * ratio, bounds.1) }
}

fn constrained(base: Base, content: (f32, f32), natural: IntrinsicSize) -> (f32, f32) {
    let Some(ratio) = ratio_of(natural) else { return content };
    match base {
        Base::Cover => cover_over(content, ratio),
        _ => contain_within(content, ratio),
    }
}

fn none_size(natural: IntrinsicSize) -> (f32, f32) {
    let ratio = ratio_of(natural);
    match (natural.width, natural.height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => (width, ratio.map_or(DEFAULT_OBJECT_SIZE.1, |ratio| width / ratio)),
        (None, Some(height)) => (ratio.map_or(DEFAULT_OBJECT_SIZE.0, |ratio| height * ratio), height),
        (None, None) => ratio.map_or(DEFAULT_OBJECT_SIZE, |ratio| contain_within(DEFAULT_OBJECT_SIZE, ratio)),
    }
}
