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

fn view_box_length(token: &str, basis: f32) -> Option<f32> {
    let token = token.trim();
    if let Some(percent) = token.strip_suffix('%') {
        return percent.trim().parse::<f32>().ok().map(|value| basis * value / 100.0);
    }
    if let Some(pixels) = token.strip_suffix("px") {
        return pixels.trim().parse::<f32>().ok();
    }
    token.parse::<f32>().ok().filter(|value| *value == 0.0)
}

fn parse_view_box(specified: &str, natural: (f32, f32)) -> Option<Rect> {
    let specified = specified.trim();
    let open = specified.find('(')?;
    let inner = specified[open + 1..].strip_suffix(')')?;
    let tokens = components(inner)?;
    let name = specified[..open].trim().to_ascii_lowercase();
    let (width, height) = natural;
    let (left, top, right, bottom) = match name.as_str() {
        "inset" => {
            let sides = tokens.iter().take_while(|token| !token.eq_ignore_ascii_case("round")).copied().collect::<Vec<_>>();
            let basis = [height, width, height, width];
            let mut lengths = Vec::new();
            if sides.len() > 4 {
                return None;
            }
            for (index, token) in sides.iter().enumerate() {
                lengths.push(view_box_length(token, basis[index])?);
            }
            let (top, right, bottom, left) = match lengths.as_slice() {
                [all] => (*all, *all, *all, *all),
                [vertical, horizontal] => (*vertical, *horizontal, *vertical, *horizontal),
                [top, horizontal, bottom] => (*top, *horizontal, *bottom, *horizontal),
                [top, right, bottom, left] => (*top, *right, *bottom, *left),
                _ => return None,
            };
            (left, top, width - right, height - bottom)
        }
        "rect" => {
            if tokens.len() < 4 {
                return None;
            }
            let side = |token: &str, basis: f32, default: f32| {
                if token.eq_ignore_ascii_case("auto") { Some(default) } else { view_box_length(token, basis) }
            };
            let top = side(tokens[0], height, 0.0)?;
            let right = side(tokens[1], width, width)?;
            let bottom = side(tokens[2], height, height)?;
            let left = side(tokens[3], width, 0.0)?;
            (left, top, right, bottom)
        }
        "xywh" => {
            if tokens.len() < 4 {
                return None;
            }
            let x = view_box_length(tokens[0], width)?;
            let y = view_box_length(tokens[1], height)?;
            let w = view_box_length(tokens[2], width)?;
            let h = view_box_length(tokens[3], height)?;
            (x, y, x + w, y + h)
        }
        _ => return None,
    };
    let rect = Rect { x: left, y: top, width: right - left, height: bottom - top };
    (rect.width > 0.0 && rect.height > 0.0 && rect.width.is_finite() && rect.height.is_finite()).then_some(rect)
}

/// The `object-view-box` of the element in natural-size coordinates, if it is not `none`.
pub(crate) fn view_box(style: &Style, natural: (f32, f32)) -> Option<Rect> {
    let specified = extra_props::stored(style, "object-view-box")?;
    if specified.trim().eq_ignore_ascii_case("none") || natural.0 <= 0.0 || natural.1 <= 0.0 {
        return None;
    }
    parse_view_box(&specified, natural)
}

/// Where the whole image is drawn inside `content` once `object-view-box` and `object-fit`
/// are applied. The view box replaces the natural size and the rest of the image stays
/// visible, clipped by the content box.
pub(crate) fn image_rect(style: &Style, content: Rect, natural: IntrinsicSize) -> Rect {
    let fit = ObjectFit::of(style);
    let natural_size = natural.default_dimensions();
    let has_natural_size = natural.width.is_some() || natural.height.is_some() || natural.ratio.is_some();
    let Some(view) = view_box(style, natural_size).filter(|_| has_natural_size) else {
        return match fit {
            Some(fit) => fit.destination(content, natural),
            None => content,
        };
    };
    let target = match fit {
        Some(fit) => fit.destination(content, IntrinsicSize { width: Some(view.width), height: Some(view.height), ratio: Some(view.width / view.height) }),
        None => content,
    };
    let scale_x = target.width / view.width;
    let scale_y = target.height / view.height;
    Rect {
        x: target.x - view.x * scale_x,
        y: target.y - view.y * scale_y,
        width: natural_size.0 * scale_x,
        height: natural_size.1 * scale_y,
    }
}

#[cfg(test)]
mod view_box_tests {
    use super::*;

    #[test]
    fn inset_view_box_selects_the_remaining_region() {
        let view = parse_view_box("inset(50px 0px 0px 0px)", (50.0, 100.0)).unwrap();
        assert_eq!((view.x, view.y, view.width, view.height), (0.0, 50.0, 50.0, 50.0));
    }

    #[test]
    fn xywh_and_rect_view_boxes_resolve_against_the_natural_size() {
        let xywh = parse_view_box("xywh(10px 20px 50% 25%)", (100.0, 200.0)).unwrap();
        assert_eq!((xywh.x, xywh.y, xywh.width, xywh.height), (10.0, 20.0, 50.0, 50.0));
        let rect = parse_view_box("rect(auto 80px 100px auto)", (100.0, 200.0)).unwrap();
        assert_eq!((rect.x, rect.y, rect.width, rect.height), (0.0, 0.0, 80.0, 100.0));
    }

    #[test]
    fn view_box_needs_a_natural_size() {
        let mut style = Style::initial();
        extra_props::set_stored(&mut style, "object-view-box", Some("inset(50px 0px 0px 0px)"));
        let content = Rect { x: 0.0, y: 0.0, width: 300.0, height: 150.0 };
        let sized = IntrinsicSize { width: Some(100.0), height: Some(100.0), ratio: Some(1.0) };
        assert_ne!(image_rect(&style, content, sized), content);
        let unsized_image = IntrinsicSize { width: None, height: None, ratio: None };
        assert_eq!(image_rect(&style, content, unsized_image), content);
    }

    #[test]
    fn empty_view_box_is_ignored() {
        assert!(parse_view_box("inset(0px 100px)", (100.0, 100.0)).is_none());
    }
}
