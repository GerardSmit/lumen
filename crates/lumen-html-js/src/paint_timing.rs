//! Paint Timing: the `first-paint` and `first-contentful-paint` entries of one document.
//!
//! The rendering update calls [`update`] after it has laid the document out. The cached display
//! list of that frame is the paint: its visible, non-transparent commands decide whether the
//! document has painted anything and whether any of it is contentful (text, images including
//! background images, drawn canvases, SVG). Gradients, plain colors and embedded documents are
//! paintable but not contentful; an embedded document reports through its own realm.
use super::*;
use lumen_html::paint::{Affine, BackgroundPaint, Command, DisplayList, ImageData, Rect, SvgPaint};
use std::cell::Cell;

const MAX_FRAME_DEPTH: usize = 8;
const MAX_PAINT_DEPTH: usize = 8;
const INK_SCAN_PIXELS: usize = 1 << 22;

#[derive(Default)]
pub(crate) struct PaintTimingState {
    first_paint: Cell<bool>,
    first_contentful_paint: Cell<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Observation {
    pub(crate) painted: bool,
    pub(crate) contentful: bool,
}

pub(crate) fn update(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    update_at(ctx, realm, 0)
}

fn update_at(ctx: &mut Ctx, realm: &Rc<DomRealm>, depth: usize) -> OpResult<()> {
    if depth < MAX_FRAME_DEPTH {
        update_children(ctx, realm, depth);
    }
    if !realm.has_browsing_context || realm.lifecycle.paint.first_contentful_paint.get() {
        return Ok(());
    }
    let seen = {
        let session = realm.session.borrow();
        let (Some(list), Some((width, height))) = (session.cached_display_list(), session.viewport_size()) else {
            return Ok(());
        };
        scan(list, width, height)
    };
    realm.lifecycle.paint.publish(ctx, seen)
}

fn update_children(ctx: &mut Ctx, realm: &Rc<DomRealm>, depth: usize) {
    let Ok(children) = realm.embedded_paint_contexts() else { return };
    for (_, _, child) in children {
        let Ok(Some(handle)) = child.child_realm_handle() else { continue };
        let has_layout = child.layout_flusher.borrow().is_some();
        let _ = ctx.with_host_realm(&handle, |ctx| {
            if has_layout {
                let _ = child.flush_layout_mode(false);
            }
            update_at(ctx, &child, depth + 1)
        });
    }
}

impl PaintTimingState {
    fn publish(&self, ctx: &mut Ctx, seen: Observation) -> OpResult<()> {
        let now = lumen_host::perf::web_now_ms();
        if (seen.painted || seen.contentful) && !self.first_paint.replace(true) {
            lumen_host::performance_timeline::record_paint(ctx, "first-paint", now)?;
        }
        if seen.contentful && !self.first_contentful_paint.replace(true) {
            lumen_host::performance_timeline::record_paint(ctx, "first-contentful-paint", now)?;
        }
        Ok(())
    }
}

/// What the display list of a `width` x `height` viewport paints. The leading full-viewport fill
/// is the canvas default, not document paint.
pub(crate) fn scan(list: &DisplayList, width: u32, height: u32) -> Observation {
    let viewport = Rect { x: 0.0, y: 0.0, width: width as f32, height: height as f32 };
    let skip = usize::from(matches!(list.0.first(), Some(Command::FillRect { rect, .. }) if *rect == viewport));
    let mut walk = Walk {
        transform: Affine::IDENTITY,
        clip: Some(viewport),
        opacity: 1.0,
        scopes: Vec::new(),
        seen: Observation::default(),
    };
    for command in list.0.iter().skip(skip) {
        walk.visit(command);
        if walk.seen.contentful {
            break;
        }
    }
    walk.seen
}

enum Scope {
    Transform(Affine),
    Clip(Option<Rect>),
    Layer { opacity: f32, clip: Option<Rect> },
}

struct Walk {
    transform: Affine,
    clip: Option<Rect>,
    opacity: f32,
    scopes: Vec<Scope>,
    seen: Observation,
}

impl Walk {
    fn paint(&mut self) {
        self.seen.painted = true;
    }

    fn content(&mut self) {
        self.seen.painted = true;
        self.seen.contentful = true;
    }

    /// The part of `rect`, in viewport space, that survives the transform and active clips.
    fn bounds(&self, rect: Rect) -> Option<Rect> {
        let m = self.transform;
        let determinant = m.a * m.d - m.b * m.c;
        if !rect.is_valid() || !determinant.is_finite() || determinant.abs() < 1e-12 {
            return None;
        }
        let corners = [
            m.apply(rect.x, rect.y),
            m.apply(rect.x + rect.width, rect.y),
            m.apply(rect.x, rect.y + rect.height),
            m.apply(rect.x + rect.width, rect.y + rect.height),
        ];
        let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in corners {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
        Rect { x: left, y: top, width: right - left, height: bottom - top }.intersection(self.clip?)
    }

    fn visible(&self, rect: Rect) -> bool {
        self.opacity > 0.0 && self.bounds(rect).is_some()
    }

    fn visit(&mut self, command: &Command) {
        match command {
            Command::PushTransform(matrix) => {
                self.scopes.push(Scope::Transform(self.transform));
                self.transform = self.transform.then(*matrix);
            }
            Command::PopTransform => {
                if let Some(Scope::Transform(previous)) = self.scopes.pop() {
                    self.transform = previous;
                }
            }
            Command::PushClip(rect) | Command::PushBoxClip(rect) => {
                self.scopes.push(Scope::Clip(self.clip));
                self.clip = self.bounds(*rect);
            }
            Command::PopClip => {
                if let Some(Scope::Clip(previous)) = self.scopes.pop() {
                    self.clip = previous;
                }
            }
            Command::PushLayer { svg_clip, rect, opacity, clip, .. } => {
                if svg_clip.as_ref().is_some_and(|owner| owner.mask.is_some()) && self.visible(*rect) {
                    self.content();
                }
                self.scopes.push(Scope::Layer { opacity: self.opacity, clip: self.clip });
                if opacity.is_finite() {
                    self.opacity *= opacity.clamp(0.0, 1.0);
                }
                if *clip {
                    self.clip = self.bounds(*rect);
                }
            }
            Command::PopLayer => {
                if let Some(Scope::Layer { opacity, clip }) = self.scopes.pop() {
                    self.opacity = opacity;
                    self.clip = clip;
                }
            }
            Command::FillRect { rect, color } | Command::FillRoundedRect { rect, color, .. } => {
                if color.a > 0 && self.visible(*rect) {
                    self.paint();
                }
            }
            Command::FillGradient { rect, .. } => {
                if self.visible(*rect) {
                    self.paint();
                }
            }
            Command::FillBackground(fill) => {
                if self.visible(fill.rect) {
                    self.background(&fill.image, 0);
                }
            }
            Command::MaskedBackground(masked) => {
                if self.visible(masked.rect) {
                    self.content();
                }
            }
            Command::BoxShadow { rect, shadow, .. } => {
                if shadow.color.a > 0 && self.visible(*rect) {
                    self.paint();
                }
            }
            Command::StrokeBorder { rect, width, color, .. } | Command::StrokePatternBorder { rect, width, color, .. } => {
                if *width > 0.0 && color.a > 0 && self.visible(*rect) {
                    self.paint();
                }
            }
            Command::StrokeBoxBorder(border) => {
                let drawn = border.widths.iter().zip(&border.colors).any(|(width, color)| *width > 0.0 && color.a > 0);
                if drawn && self.visible(border.rect) {
                    self.paint();
                }
            }
            Command::GlyphRun { origin_x, baseline_y, size, color, glyphs } => {
                if color.a > 0 && *size > 0.0 && !glyphs.is_empty() {
                    let extent = glyphs.iter().fold(0.0f32, |extent, glyph| extent.max(glyph.x));
                    let rect = Rect { x: *origin_x, y: *baseline_y - *size, width: extent + *size, height: *size * 1.4 };
                    if self.visible(rect) {
                        self.content();
                    }
                }
            }
            Command::Image { rect, image } => {
                if self.visible(*rect) && has_ink(image) {
                    self.content();
                }
            }
            Command::ReservedImage { .. } => {}
            Command::SvgPath { bounds, fill, stroke, stroke_width, .. } => {
                let inked = |paint: &Option<SvgPaint>| match paint {
                    Some(SvgPaint::Color(color)) => color.a > 0,
                    Some(SvgPaint::Gradient(_)) => true,
                    None => false,
                };
                let drawn = inked(fill) || (*stroke_width > 0.0 && inked(stroke));
                if drawn && self.opacity > 0.0 && bounds.is_valid() && self.clip.is_some() {
                    self.content();
                }
            }
        }
    }

    fn background(&mut self, paint: &BackgroundPaint, depth: usize) {
        if depth > MAX_PAINT_DEPTH {
            return;
        }
        match paint {
            BackgroundPaint::Solid(color) => {
                if color.a > 0 {
                    self.paint();
                }
            }
            BackgroundPaint::Gradient(_) | BackgroundPaint::Worklet(_) => self.paint(),
            BackgroundPaint::Image(_) => self.content(),
            BackgroundPaint::Border(border) => {
                if border.widths.iter().any(|width| *width > 0.0) {
                    self.background(&border.image, depth + 1);
                }
            }
            BackgroundPaint::CrossFade(layers) => {
                for (layer, weight) in layers.iter() {
                    if *weight > 0.0 {
                        self.background(layer, depth + 1);
                    }
                }
            }
        }
    }
}

/// A canvas or image bitmap with any non-transparent pixel. Bitmaps past the scan budget are
/// real pictures, never a blank canvas.
fn has_ink(image: &ImageData) -> bool {
    image.pixels.len() / 4 > INK_SCAN_PIXELS || image.pixels.chunks_exact(4).any(|pixel| pixel[3] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_host::Extension;
    use lumen_html::paint::Rgba;
    use std::sync::Arc;

    const RED: Rgba = Rgba { r: 255, g: 0, b: 0, a: 255 };
    const WHITE: Rgba = Rgba { r: 255, g: 255, b: 255, a: 255 };

    fn rect(x: f32, y: f32, width: f32, height: f32) -> Rect {
        Rect { x, y, width, height }
    }

    fn canvas() -> Command {
        Command::FillRect { rect: rect(0.0, 0.0, 100.0, 100.0), color: WHITE }
    }

    fn fill(x: f32, y: f32, width: f32, height: f32, color: Rgba) -> Command {
        Command::FillRect { rect: rect(x, y, width, height), color }
    }

    fn image(alpha: u8) -> Arc<ImageData> {
        Arc::new(ImageData { width: 2, height: 2, pixels: [0, 0, 0, alpha].repeat(4) })
    }

    fn layer(opacity: f32, clip: bool) -> Command {
        Command::PushLayer {
            svg_clip: None,
            filters: None,
            corners: None,
            rect: rect(0.0, 0.0, 100.0, 100.0),
            radius: 0.0,
            opacity,
            clip,
        }
    }

    fn observe(commands: Vec<Command>) -> Observation {
        scan(&DisplayList(commands), 100, 100)
    }

    const BLANK: Observation = Observation { painted: false, contentful: false };
    const PAINTED: Observation = Observation { painted: true, contentful: false };
    const CONTENTFUL: Observation = Observation { painted: true, contentful: true };

    #[test]
    fn the_default_canvas_fill_alone_is_not_a_paint() {
        assert_eq!(observe(vec![canvas()]), BLANK);
        assert_eq!(observe(vec![]), BLANK);
    }

    #[test]
    fn a_background_color_paints_without_being_contentful() {
        assert_eq!(observe(vec![canvas(), fill(0.0, 0.0, 10.0, 10.0, RED)]), PAINTED);
        let transparent = Rgba { a: 0, ..RED };
        assert_eq!(observe(vec![canvas(), fill(0.0, 0.0, 10.0, 10.0, transparent)]), BLANK);
    }

    #[test]
    fn a_propagated_root_background_is_a_paint() {
        assert_eq!(observe(vec![canvas(), fill(0.0, 0.0, 100.0, 100.0, RED)]), PAINTED);
    }

    #[test]
    fn images_are_contentful_only_with_ink_and_inside_the_viewport() {
        let picture = |alpha, x| vec![canvas(), Command::Image { rect: rect(x, 0.0, 10.0, 10.0), image: image(alpha) }];
        assert_eq!(observe(picture(255, 0.0)), CONTENTFUL);
        assert_eq!(observe(picture(0, 0.0)), BLANK);
        assert_eq!(observe(picture(255, -1000.0)), BLANK);
        assert_eq!(observe(picture(255, 100.0)), BLANK);
    }

    #[test]
    fn background_images_are_contentful_but_gradients_are_not() {
        let background = |image| {
            Command::FillBackground(Box::new(lumen_html::paint::BackgroundFill {
                corners: None,
                rect: rect(0.0, 0.0, 50.0, 50.0),
                radius: 0.0,
                positioning_rect: rect(0.0, 0.0, 50.0, 50.0),
                image_rect: rect(0.0, 0.0, 50.0, 50.0),
                repeat: [lumen_html::paint::BackgroundRepeat::Repeat; 2],
                image,
            }))
        };
        assert_eq!(observe(vec![canvas(), background(BackgroundPaint::Image(image(255)))]), CONTENTFUL);
        assert_eq!(observe(vec![canvas(), background(BackgroundPaint::Solid(RED))]), PAINTED);
    }

    #[test]
    fn zero_opacity_scale_zero_and_empty_clips_hide_content() {
        let picture = Command::Image { rect: rect(0.0, 0.0, 10.0, 10.0), image: image(255) };
        let hidden_by_opacity = vec![canvas(), layer(0.0, false), picture.clone(), Command::PopLayer];
        assert_eq!(observe(hidden_by_opacity), BLANK);
        let faint = vec![canvas(), layer(0.1, false), picture.clone(), Command::PopLayer];
        assert_eq!(observe(faint), CONTENTFUL);
        let collapsed = Affine { a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, f: 0.0 };
        let scaled_away = vec![canvas(), Command::PushTransform(collapsed), picture.clone(), Command::PopTransform];
        assert_eq!(observe(scaled_away), BLANK);
        let clipped_out = vec![canvas(), Command::PushClip(rect(50.0, 50.0, 10.0, 10.0)), picture.clone(), Command::PopClip];
        assert_eq!(observe(clipped_out), BLANK);
        let after_pop = vec![canvas(), layer(0.0, false), Command::PopLayer, picture];
        assert_eq!(observe(after_pop), CONTENTFUL);
    }

    #[test]
    fn translated_content_is_judged_in_viewport_space() {
        let picture = Command::Image { rect: rect(0.0, 0.0, 10.0, 10.0), image: image(255) };
        let shift = |x| Affine { e: x, ..Affine::IDENTITY };
        let off = vec![canvas(), Command::PushTransform(shift(-1000.0)), picture.clone(), Command::PopTransform];
        assert_eq!(observe(off), BLANK);
        let on = vec![canvas(), Command::PushTransform(shift(40.0)), picture, Command::PopTransform];
        assert_eq!(observe(on), CONTENTFUL);
    }

    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
            Err(())
        }
        fn ascent(&self, size: f32) -> f32 {
            size * 0.8
        }
        fn line_height(&self, size: f32) -> f32 {
            size * 1.2
        }
    }

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let stack = engine.ctx().get_member(&error, "stack").ok().and_then(|stack| engine.ctx().coerce_string(&stack).ok());
                panic!("{stack:?}\nSource: {source}");
            }
        }
    }

    #[test]
    fn rendering_updates_publish_first_paint_then_first_contentful_paint_once() {
        let mut engine = Engine::new();
        lumen_host::install(&mut engine, &[Extension {
            modules: &[lumen_host::performance::install_globals],
            ..Extension::new("performance")
        }]);
        let realm = crate::install(engine.ctx(), "<style>body{margin:0}</style><div id=box style='width:20px;height:20px'></div>", 64).unwrap();
        lumen_host::install(&mut engine, &[crate::performance_extension()]);
        realm.set_layout_flusher(Rc::new(|session| {
            session.display_list(100, 100, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        let names = |engine: &mut Engine| match eval(engine, "performance.getEntriesByType('paint').map(entry => entry.name).join()") {
            Value::Str(text) => text.as_str().to_string(),
            _ => panic!("entry names are not a string"),
        };
        assert!(matches!(
            eval(&mut engine, "typeof PerformancePaintTiming === 'function' && PerformanceObserver.supportedEntryTypes.includes('paint')"),
            Value::Bool(true)
        ));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert_eq!(names(&mut engine), "");
        eval(&mut engine, "document.getElementById('box').style.background = 'red'; 0");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert_eq!(names(&mut engine), "first-paint");
        eval(&mut engine, "document.body.insertAdjacentHTML('beforeend', '<svg width=20 height=20><circle cx=10 cy=10 r=8 fill=green /></svg>'); 0");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert_eq!(names(&mut engine), "first-paint,first-contentful-paint");
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                const [paint, contentful] = performance.getEntriesByType('paint');
                return paint instanceof PerformancePaintTiming && paint.entryType === 'paint' && paint.duration === 0
                    && contentful.startTime >= paint.startTime
                    && performance.getEntriesByName('first-contentful-paint').length === 1
                    && (() => { try { new PerformancePaintTiming(); return false; } catch (e) { return e instanceof TypeError; } })();
            })()"#),
            Value::Bool(true)
        ));
    }
}
