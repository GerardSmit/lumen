//! Retained document paint state. Scale-only changes replay the same CSS display list.
use crate::{
    css::{self, Style, StyleIndex},
    layout::{self, ImageResolver, LayoutError},
    paint::{DisplayList, TextShaper},
    Document, MutationKind, NodeId, NodeKind,
};
use alloc::vec::Vec;

struct CachedFrame {
    document_version: u64,
    width: u32,
    height: u32,
    image_generation: Option<u64>,
    list: DisplayList,
    geometry: layout::LayoutGeometry,
}

fn in_style(document: &Document, mut id: NodeId) -> bool {
    loop {
        match document.kind(id) {
            Ok(NodeKind::Element { name, .. }) if name == "style" => return true,
            Err(_) => return true,
            _ => {}
        }
        match document.parent(id) {
            Ok(Some(parent)) => id = parent,
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}

pub(crate) fn subtree_has_style(document: &Document, root: NodeId) -> bool {
    let mut current = root;
    loop {
        match document.kind(current) {
            Ok(NodeKind::Element { name, .. }) if name == "style" => return true,
            Err(_) => return true,
            _ => {}
        }
        if let Ok(Some(child)) = document.first_child(current) {
            current = child;
            continue;
        }
        loop {
            if current == root {
                return false;
            }
            if let Ok(Some(sibling)) = document.next_sibling(current) {
                current = sibling;
                break;
            }
            match document.parent(current) {
                Ok(Some(parent)) => current = parent,
                _ => return true,
            }
        }
    }
}

pub struct RenderSession {
    document: Document,
    rules: Option<StyleIndex>,
    rules_version: u64,
    cached: Option<CachedFrame>,
    scrolls: Vec<layout::ScrollOffset>,
}

impl RenderSession {
    pub fn new(document: Document) -> Self {
        Self {
            document,
            rules: None,
            rules_version: 0,
            cached: None,
            scrolls: Vec::new(),
        }
    }
    pub fn document(&self) -> &Document {
        &self.document
    }
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    pub fn set_media_environment(
        &mut self,
        environment: css::MediaEnvironment,
    ) -> Result<(), LayoutError> {
        if !environment.width.is_finite()
            || !environment.height.is_finite()
            || !environment.resolution.is_finite()
            || environment.width < 0.0
            || environment.height < 0.0
            || environment.resolution <= 0.0
        {
            return Err(LayoutError::Css(css::CssError {
                offset: 0,
                message: "invalid media environment",
            }));
        }
        self.refresh_rules()?;
        self.rules
            .as_mut()
            .expect("stylesheets initialized")
            .environment = environment;
        self.cached = None;
        Ok(())
    }

    pub fn computed_style(&mut self, node: NodeId) -> Result<Style, LayoutError> {
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules()?;
        let mut ancestors = Vec::new();
        let mut current = Some(node);
        while let Some(id) = current {
            if ancestors.len() >= 512 {
                return Err(LayoutError::DepthLimit);
            }
            if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
                ancestors
                    .try_reserve(1)
                    .map_err(|_| LayoutError::CommandLimit)?;
                ancestors.push(id);
            }
            current = self
                .document
                .parent(id)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        let mut style = Style::initial();
        for id in ancestors.into_iter().rev() {
            style = css::compute_node(
                &self.document,
                id,
                Some(&style),
                self.rules.as_ref().expect("stylesheets initialized"),
            )
            .map_err(LayoutError::Css)?;
        }
        Ok(style)
    }

    fn refresh_rules(&mut self) -> Result<(), LayoutError> {
        let version = self.document.version();
        if self.rules.is_none() || self.rules_version != version {
            let rules_changed = self.rules.is_none()
                || (self.document.mutations().is_empty() && self.rules_version != version)
                || self
                    .document
                    .mutations()
                    .iter()
                    .any(|mutation| match &mutation.kind {
                        MutationKind::FullRebuild => true,
                        MutationKind::CharacterData => in_style(&self.document, mutation.target),
                        MutationKind::Tree {
                            added,
                            removed,
                            styles_changed,
                        } => {
                            *styles_changed
                                || in_style(&self.document, mutation.target)
                                || added.is_some_and(|id| subtree_has_style(&self.document, id))
                                || removed.is_some_and(|id| subtree_has_style(&self.document, id))
                        }
                        MutationKind::Attribute(_) => false,
                    });
            if rules_changed {
                let environment = self
                    .rules
                    .as_ref()
                    .map(|rules| rules.environment)
                    .unwrap_or_default();
                let mut rules = layout::stylesheets(&self.document)?;
                rules.environment = environment;
                self.rules = Some(rules);
            }
            self.rules_version = version;
        }
        Ok(())
    }
    pub fn invalidate_fonts(&mut self) {
        self.cached = None;
    }

    /// Hit-test the last completed layout in CSS pixels. Mutated documents need a new frame.
    pub fn hit_test(&self, x_css: f32, y_css: f32) -> Option<NodeId> {
        let frame = self.cached.as_ref()?;
        if frame.document_version != self.document.version()
            || !x_css.is_finite()
            || !y_css.is_finite()
            || x_css < 0.0
            || y_css < 0.0
            || x_css >= frame.width as f32
            || y_css >= frame.height as f32
        {
            return None;
        }
        frame
            .geometry
            .hits
            .iter()
            .enumerate()
            .rev()
            .find(|(index, _)| frame.geometry.contains_hit(*index, x_css, y_css))
            .map(|(_, hit)| hit.node)
    }

    pub fn scroll_offset(&self, node: NodeId) -> (f32, f32) {
        self.scrolls
            .iter()
            .find(|scroll| scroll.node == node)
            .map_or((0.0, 0.0), |scroll| (scroll.x, scroll.y))
    }

    /// Set and clamp a scroll container's offset using its most recent layout extent.
    pub fn set_scroll_offset(
        &mut self,
        node: NodeId,
        x_css: f32,
        y_css: f32,
    ) -> Result<bool, LayoutError> {
        self.document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?;
        if !x_css.is_finite() || !y_css.is_finite() {
            return Err(LayoutError::InvalidTree);
        }
        let Some(extent) = self.cached.as_ref().and_then(|frame| {
            frame
                .geometry
                .scroll_extents
                .iter()
                .find(|extent| extent.node == node)
        }) else {
            return Ok(false);
        };
        let (x, y) = (x_css.clamp(0.0, extent.x), y_css.clamp(0.0, extent.y));
        if self.scroll_offset(node) == (x, y) {
            return Ok(false);
        }
        if let Some(scroll) = self.scrolls.iter_mut().find(|scroll| scroll.node == node) {
            scroll.x = x;
            scroll.y = y;
        } else {
            self.scrolls
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            self.scrolls.push(layout::ScrollOffset { node, x, y });
        }
        self.cached = None;
        Ok(true)
    }

    pub fn display_list(
        &mut self,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
    ) -> Result<&DisplayList, LayoutError> {
        self.render(width, height, text, None)
    }

    pub fn display_list_with_images(
        &mut self,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
        images: &dyn ImageResolver,
    ) -> Result<&DisplayList, LayoutError> {
        self.render(width, height, text, Some(images))
    }

    fn render(
        &mut self,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
        images: Option<&dyn ImageResolver>,
    ) -> Result<&DisplayList, LayoutError> {
        self.scrolls
            .retain(|scroll| self.document.kind(scroll.node).is_ok());
        let version = self.document.version();
        let image_generation = images.map(ImageResolver::generation);
        let fresh = self.cached.as_ref().is_some_and(|cached| {
            cached.document_version == version
                && cached.width == width
                && cached.height == height
                && cached.image_generation == image_generation
        });
        if !fresh {
            self.refresh_rules()?;
            let rules = self.rules.as_mut().expect("stylesheets initialized");
            rules.environment = css::MediaEnvironment {
                width: width as f32,
                height: height as f32,
                ..rules.environment
            };
            let mut geometry = layout::LayoutGeometry::default();
            let list = layout::display_list_with_styles(
                &self.document,
                width,
                height,
                text,
                self.rules.as_ref().expect("stylesheets initialized"),
                images,
                Some(&mut geometry),
                &self.scrolls,
            )?;
            self.cached = Some(CachedFrame {
                document_version: version,
                width,
                height,
                image_generation,
                list,
                geometry,
            });
            self.document.clear_mutations();
        }
        Ok(&self.cached.as_ref().expect("cached after layout").list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        layout::ImageState,
        paint::{ImageData, ShapedRun},
        NodeKind,
    };
    use alloc::sync::Arc;
    use core::cell::Cell;

    struct NoText;

    #[test]
    fn media_environment_survives_viewport_and_stylesheet_refresh() {
        let document = crate::html::parse("<style>@media screen and (min-width:20px){#target{width:7px}}@media print and (min-resolution:2dppx){#target{width:9px}}</style><div id='target'></div>",24).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(10, 10, &NoText).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, None);
        session.display_list(20, 10, &NoText).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(7.0));
        session
            .set_media_environment(css::MediaEnvironment {
                print: true,
                resolution: 2.0,
                ..Default::default()
            })
            .unwrap();
        session.display_list(30, 10, &NoText).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(9.0));
        session
            .document_mut()
            .set_attribute(target, "class", "changed")
            .unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(9.0));
        let sheet =
            crate::selector::query_selector(session.document(), session.document().root(), "style")
                .unwrap()
                .unwrap();
        session.document_mut().remove(sheet).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, None);
        let environment = session.rules.as_ref().unwrap().environment;
        assert!(environment.print);
        assert_eq!(environment.resolution, 2.0);
        assert_eq!(environment.width, 30.0);
        assert!(session
            .set_media_environment(css::MediaEnvironment {
                resolution: f32::NAN,
                ..Default::default()
            })
            .is_err());
    }

    #[test]
    fn rounded_overflow_hit_test_excludes_child_corners_after_flex_placement() {
        let document = crate::html::parse("<div style='display:flex;gap:5px'><div style='width:5px;height:10px'></div><div id='outer' style='width:10px;height:10px;overflow:hidden;border-radius:5px'><div id='inner' style='width:10px;height:10px'></div></div></div>", 32).unwrap();
        let outer = crate::selector::query_selector(&document, document.root(), "#outer")
            .unwrap()
            .unwrap();
        let inner = crate::selector::query_selector(&document, document.root(), "#inner")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(40, 20, &NoText).unwrap();
        assert_eq!(session.hit_test(10.5, 0.5), Some(outer));
        assert_eq!(session.hit_test(15.0, 5.0), Some(inner));
    }

    #[test]
    fn computed_style_and_layout_share_contextual_selector_matching() {
        let document = crate::html::parse("<style>.outer > p{background:red;width:3px;height:2px}.outer p{color:blue}</style><div class='outer'><p id='target'></p></div>", 24).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let style = session.computed_style(target).unwrap();
        assert_eq!(style.color.b, 255);
        assert_eq!(style.background.r, 255);
        let list = session.display_list(10, 10, &NoText).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, crate::paint::Command::FillRect { rect, color } if rect.width == 3.0 && rect.height == 2.0 && color.r == 255)));
    }

    #[test]
    fn hit_test_uses_deepest_box_viewport_and_completed_version() {
        let document = crate::html::parse(
            "<div id='outer' style='width:20px;height:20px;padding:2px'><div id='inner' style='width:5px;height:5px'></div><div id='hidden' style='display:none;width:20px;height:20px'></div></div>", 16).unwrap();
        let outer = crate::selector::query_selector(&document, document.root(), "#outer")
            .unwrap()
            .unwrap();
        let inner = crate::selector::query_selector(&document, document.root(), "#inner")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        assert_eq!(session.hit_test(3.0, 3.0), None);
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(3.0, 3.0), Some(inner));
        assert_eq!(session.hit_test(1.0, 1.0), Some(outer));
        assert_eq!(session.hit_test(7.0, 3.0), Some(outer));
        assert_eq!(session.hit_test(30.0, 3.0), None);
        assert_eq!(session.hit_test(f32::NAN, 3.0), None);
        session
            .document_mut()
            .set_attribute(inner, "style", "display:none")
            .unwrap();
        assert_eq!(session.hit_test(3.0, 3.0), None);
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(3.0, 3.0), Some(outer));
    }

    #[test]
    fn scroll_offsets_clamp_and_hit_test_respects_overflow() {
        let document = crate::html::parse(
            "<div id='scroll' style='overflow:auto;width:10px;height:10px'><div id='first' style='height:10px;background:red'></div><div id='second' style='height:10px;background:blue'></div></div>", 16).unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let container = find("#scroll");
        let first = find("#first");
        let second = find("#second");
        let mut session = RenderSession::new(document);
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(5.0, 5.0), Some(first));
        assert_ne!(session.hit_test(5.0, 15.0), Some(second));
        assert!(session.set_scroll_offset(container, 100.0, 100.0).unwrap());
        assert_eq!(session.scroll_offset(container), (0.0, 10.0));
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(5.0, 5.0), Some(second));
        assert!(!session.set_scroll_offset(container, 0.0, 10.0).unwrap());
        assert!(session.set_scroll_offset(container, -1.0, -1.0).unwrap());
        assert_eq!(session.scroll_offset(container), (0.0, 0.0));
    }

    #[test]
    fn transformed_hits_follow_nested_rotation_clipping_and_scroll() {
        let document = crate::html::parse("<div id='outer' style='width:20px;height:20px;transform:translate(20px,10px);overflow:hidden;border-radius:5px'><div id='inner' style='width:20px;height:20px;transform:rotate(90deg);background:red'></div></div>",16).unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let outer = find("#outer");
        let inner = find("#inner");
        let mut session = RenderSession::new(document);
        session.display_list(60, 60, &NoText).unwrap();
        assert_ne!(session.hit_test(15.0, 15.0), Some(inner));
        assert_eq!(session.hit_test(25.0, 15.0), Some(inner));
        assert_eq!(session.hit_test(20.1, 10.1), Some(outer));
        assert_ne!(session.hit_test(20.1, 10.1), Some(inner));

        let document=crate::html::parse("<div id='scroll' style='width:10px;height:10px;overflow:auto;transform:translate(20px,10px)'><div style='height:10px'></div><div id='second' style='height:10px;transform:scale(1);background:red'></div></div>",16).unwrap();
        let scroll = crate::selector::query_selector(&document, document.root(), "#scroll")
            .unwrap()
            .unwrap();
        let second = crate::selector::query_selector(&document, document.root(), "#second")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(60, 60, &NoText).unwrap();
        session.set_scroll_offset(scroll, 0.0, 10.0).unwrap();
        session.display_list(60, 60, &NoText).unwrap();
        assert_eq!(session.hit_test(25.0, 15.0), Some(second));
        assert_ne!(session.hit_test(25.0, 25.0), Some(second));
    }

    #[test]
    fn transforms_establish_fixed_containing_block_and_keep_offscreen_paint() {
        let document=crate::html::parse("<div style='width:20px;height:20px;margin-left:100px;transform:translateX(-90px)'><div id='fixed' style='position:fixed;left:0;top:0;width:5px;height:5px;background:red'></div></div>",16).unwrap();
        let fixed = crate::selector::query_selector(&document, document.root(), "#fixed")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let list = session.display_list(40, 40, &NoText).unwrap();
        assert!(list.0.iter().any(|command|matches!(command,crate::paint::Command::FillRect{color,..} if color.r==255 && color.g==0)));
        assert_eq!(session.hit_test(12.0, 2.0), Some(fixed));
        assert_ne!(session.hit_test(2.0, 2.0), Some(fixed));
    }

    #[test]
    fn transformed_flex_stretch_uses_final_origin_and_singular_boxes_do_not_hit() {
        let document=crate::html::parse("<div style='display:flex;width:40px;height:20px'><div id='item' style='width:10px;transform:rotate(90deg);background:red'></div></div><div id='singular' style='width:10px;height:10px;transform:scale(0);background:blue'></div>",16).unwrap();
        let item = crate::selector::query_selector(&document, document.root(), "#item")
            .unwrap()
            .unwrap();
        let singular = crate::selector::query_selector(&document, document.root(), "#singular")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let list = session.display_list(60, 60, &NoText).unwrap();
        let matrix = list
            .0
            .iter()
            .find_map(|command| match command {
                crate::paint::Command::PushTransform(matrix) => Some(*matrix),
                _ => None,
            })
            .unwrap();
        let point = matrix.apply(0.0, 0.0);
        assert!((point.0 - 15.0).abs() < 0.001 && (point.1 - 5.0).abs() < 0.001);
        assert_eq!(session.hit_test(12.0, 10.0), Some(item));
        assert_ne!(session.hit_test(5.0, 25.0), Some(singular));
    }
    impl TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
            Ok(ShapedRun {
                glyphs: alloc::vec![],
                width: 0.0,
            })
        }
        fn ascent(&self, _: f32) -> f32 {
            0.0
        }
        fn line_height(&self, _: f32) -> f32 {
            0.0
        }
    }

    #[test]
    fn reuses_unchanged_frame_and_rebuilds_on_mutation() {
        let document =
            crate::html::parse("<div style='background:red;width:2px;height:1px'></div>", 8)
                .unwrap();
        let mut session = RenderSession::new(document);
        let first = session.display_list(3, 1, &NoText).unwrap() as *const DisplayList;
        let second = session.display_list(3, 1, &NoText).unwrap() as *const DisplayList;
        assert_eq!(first, second);
        let root = session.document().root();
        let html = session.document().first_child(root).unwrap().unwrap();
        let head = session.document().first_child(html).unwrap().unwrap();
        let body = session.document().next_sibling(head).unwrap().unwrap();
        let div = session.document().first_child(body).unwrap().unwrap();
        assert!(matches!(
            session.document().kind(div).unwrap(),
            NodeKind::Element { .. }
        ));
        session
            .document_mut()
            .set_attribute(div, "style", "background:blue;width:2px;height:1px")
            .unwrap();
        let list = session.display_list(3, 1, &NoText).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, crate::paint::Command::FillRect { color, .. } if color.b == 255 && color.r == 0)));
    }

    #[test]
    fn stylesheet_text_mutation_rebuilds_rules() {
        let document = crate::html::parse(
            "<head><style>p{color:red}</style></head><body><p>x</p></body>",
            10,
        )
        .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(30, 30, &NoText).unwrap();
        let html = session
            .document()
            .first_child(session.document().root())
            .unwrap()
            .unwrap();
        let head = session.document().first_child(html).unwrap().unwrap();
        let style = session.document().first_child(head).unwrap().unwrap();
        let text = session.document().first_child(style).unwrap().unwrap();
        session
            .document_mut()
            .replace_data(text, "p{color:blue}")
            .unwrap();
        let list = session.display_list(30, 30, &NoText).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, crate::paint::Command::GlyphRun { color, .. } if color.b == 255 && color.r == 0)));
    }

    struct Images {
        generation: Cell<u64>,
        ready: Cell<bool>,
        calls: Cell<u32>,
        image: Arc<ImageData>,
    }

    impl ImageResolver for Images {
        fn resolve(&self, _: &str) -> ImageState {
            self.calls.set(self.calls.get() + 1);
            if self.ready.get() {
                ImageState::Ready(self.image.clone())
            } else {
                ImageState::Pending
            }
        }
        fn generation(&self) -> u64 {
            self.generation.get()
        }
    }

    #[test]
    fn image_generation_controls_retained_display_list() {
        let document = crate::html::parse("<img src='tile.png' width='1' height='1'>", 8).unwrap();
        let mut session = RenderSession::new(document);
        let images = Images {
            generation: Cell::new(0),
            ready: Cell::new(false),
            calls: Cell::new(0),
            image: Arc::new(ImageData {
                width: 1,
                height: 1,
                pixels: alloc::vec![1, 2, 3, 255],
            }),
        };
        assert_eq!(
            session.display_list_with_images(1, 1, &NoText, &images),
            Err(LayoutError::ImagePending)
        );
        images.ready.set(true);
        session
            .display_list_with_images(1, 1, &NoText, &images)
            .unwrap();
        session
            .display_list_with_images(1, 1, &NoText, &images)
            .unwrap();
        assert_eq!(images.calls.get(), 2);
        images.generation.set(1);
        session
            .display_list_with_images(1, 1, &NoText, &images)
            .unwrap();
        assert_eq!(images.calls.get(), 3);
    }
}
