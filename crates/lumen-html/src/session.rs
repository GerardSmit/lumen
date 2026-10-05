//! Retained document paint state. Scale-only changes replay the same CSS display list.
use crate::{
    css::{self, Style, StyleIndex},
    layout::{self, ImageResolver, LayoutError},
    paint::{Affine, DisplayList, ImageData, Rect, Rgba, TextShaper},
    Document, MutationKind, NodeId, NodeKind,
};
use alloc::{string::String, sync::Arc, vec::Vec};
use core::cell::RefCell;

struct CachedFrame {
    document_version: u64,
    width: u32,
    height: u32,
    image_generation: Option<u64>,
    list: DisplayList,
    geometry: layout::LayoutGeometry,
}

struct NodeImages<'a> {
    bitmaps: &'a [(NodeId, Arc<ImageData>)],
    fallback: Option<&'a dyn ImageResolver>,
}

impl ImageResolver for NodeImages<'_> {
    fn resolve(&self, source: &str) -> layout::ImageState {
        self.fallback
            .map_or(layout::ImageState::Failed, |images| images.resolve(source))
    }
    fn resolve_from(&self, base: &str, source: &str) -> layout::ImageState {
        self.fallback.map_or(layout::ImageState::Failed, |images| {
            images.resolve_from(base, source)
        })
    }
    fn origin_clean_from(&self, base: &str, source: &str) -> bool {
        self.fallback
            .is_none_or(|images| images.origin_clean_from(base, source))
    }
    fn resolve_node_from(
        &self,
        node: NodeId,
        base: &str,
        source: &str,
    ) -> Option<layout::ImageState> {
        self.fallback
            .and_then(|images| images.resolve_node_from(node, base, source))
    }
    fn node_origin_clean(&self, node: NodeId, base: &str, source: &str) -> bool {
        self.fallback
            .is_none_or(|images| images.node_origin_clean(node, base, source))
    }
    fn resolve_node(&self, node: NodeId) -> Option<layout::ImageState> {
        self.bitmaps
            .iter()
            .find(|(id, _)| *id == node)
            .map(|(_, image)| layout::ImageState::Ready(image.clone()))
            .or_else(|| self.fallback.and_then(|images| images.resolve_node(node)))
    }
    fn generation(&self) -> u64 {
        self.fallback
            .map_or(0, |images| ImageResolver::generation(images))
    }
}

fn in_style(document: &Document, mut id: NodeId) -> bool {
    loop {
        match document.kind(id) {
            Ok(NodeKind::Element { name, .. }) if name == "style" || name == "link" => return true,
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
            Ok(NodeKind::Element { name, .. }) if name == "style" || name == "link" => return true,
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

fn slot_assignment_host(document: &Document, target: NodeId, attribute: &str) -> Option<NodeId> {
    match attribute {
        "slot" => {
            let parent = document.parent(target).ok().flatten()?;
            document.shadow_root(parent).ok().flatten().map(|_| parent)
        }
        "name" if document.is_slot(target).ok()? => {
            let root = document.root_node(target, false).ok()?;
            document.shadow_host(root).ok().flatten()
        }
        _ => None,
    }
}

fn changed_declarations_paint_only(
    previous: &[(String, String)],
    next: &[(String, String)],
) -> bool {
    let differs = |from: &[(String, String)], to: &[(String, String)]| {
        from.iter()
            .filter(|(name, value)| !to.iter().any(|(n, v)| n == name && v == value))
            .all(|(name, _)| css::paint_only_property(name))
    };
    differs(previous, next) && differs(next, previous)
}

pub type AnimationStyleInput = (NodeId, Option<NodeId>, [Option<Arc<str>>; 9]);

/// Immutable CSS animation inputs, shared across rendering opportunities.
pub struct AnimationSnapshot {
    pub generation: u64,
    pub nodes: Vec<AnimationStyleInput>,
    pub keyframes: Vec<css::KeyframesRuleText>,
    document_version: u64,
    environment: css::MediaEnvironment,
}

pub struct RenderSession {
    document: Document,
    rules: Option<StyleIndex>,
    font_face_rules: Vec<css::FontFaceRule>,
    font_face_generation: u64,
    font_face_descriptor_overrides: Vec<(css::FontFaceIdentity, css::FontFaceDescriptors)>,
    rules_version: u64,
    cached: Option<CachedFrame>,
    frame_id: u64,
    font_generation: u64,
    validity_generation: u64,
    interaction_generation: u64,
    auto_directionality_scan: Option<(u64, bool)>,
    node_bitmaps: Vec<(NodeId, Arc<ImageData>)>,
    paint_revision: u64,
    canvas_background: Option<Rgba>,
    scrolls: Vec<layout::ScrollOffset>,
    adopted_stylesheets: Vec<(Option<NodeId>, Vec<String>)>,
    linked_stylesheets: Vec<(NodeId, String, String)>,
    stylesheet_sources: Vec<layout::LoadedStylesheet>,
    document_base_url: Option<Arc<str>>,
    animated_styles: Vec<(NodeId, Vec<(String, String)>)>,
    style_cache: RefCell<css::StyleCache>,
    layout_cache: layout::RetainedLayoutCache,
    style_version: u64,
    style_environment: Option<css::MediaEnvironment>,
    presentation_root: Option<NodeId>,
    animation_snapshot: Option<Arc<AnimationSnapshot>>,
    animation_generation: u64,
    animation_epoch: u64,
    pending_animation_nodes: Vec<NodeId>,
    pending_animation_full: bool,
    rules_generation: u64,
    read_style_cache: css::StyleCache,
    read_style_key: Option<ReadStyleKey>,
}

/// Inputs that validate cached live computed-style reads; any difference
/// discards the read cache.
#[derive(Clone, Copy, PartialEq)]
struct ReadStyleKey {
    document_version: u64,
    rules_generation: u64,
    validity_generation: u64,
    interaction_generation: u64,
    animation_epoch: u64,
    environment: css::MediaEnvironment,
}

/// One retained overflow clip that applies to a node. `transforms` maps the
/// clip's layout coordinates to viewport coordinates in retained order.
#[derive(Clone, Debug)]
pub struct RetainedOverflowClip {
    pub rect: Rect,
    /// Top-left, top-right, bottom-right, bottom-left elliptical radii.
    pub corners: Option<[[f32; 2]; 4]>,
    pub radius: f32,
    pub transforms: Vec<Affine>,
}

impl RenderSession {
    pub fn new(document: Document) -> Self {
        let interaction_generation = document.interaction_generation();
        Self {
            document,
            rules: None,
            font_face_rules: Vec::new(),
            font_face_generation: 0,
            font_face_descriptor_overrides: Vec::new(),
            rules_version: 0,
            cached: None,
            frame_id: 0,
            font_generation: 0,
            validity_generation: 0,
            interaction_generation,
            auto_directionality_scan: None,
            node_bitmaps: Vec::new(),
            paint_revision: 0,
            canvas_background: Some(layout::DEFAULT_CANVAS_BACKGROUND),
            scrolls: Vec::new(),
            adopted_stylesheets: Vec::new(),
            linked_stylesheets: Vec::new(),
            stylesheet_sources: Vec::new(),
            document_base_url: None,
            animated_styles: Vec::new(),
            style_cache: RefCell::default(),
            layout_cache: layout::RetainedLayoutCache::default(),
            style_version: 0,
            style_environment: None,
            presentation_root: None,
            animation_snapshot: None,
            animation_generation: 0,
            animation_epoch: 0,
            pending_animation_nodes: Vec::new(),
            pending_animation_full: false,
            rules_generation: 0,
            read_style_cache: css::StyleCache::default(),
            read_style_key: None,
        }
    }
    /// Changes whenever the cached display list is rebuilt or invalidated.
    pub fn frame_id(&self) -> u64 {
        self.frame_id
    }
    /// Host invalidation includes paint changes that do not mutate the DOM.
    pub fn paint_revision(&self) -> u64 {
        self.paint_revision
    }

    /// Choose the backing canvas color; image documents use `None` for transparency.
    pub fn set_canvas_background(&mut self, background: Option<Rgba>) {
        if self.canvas_background != background {
            self.canvas_background = background;
            self.invalidate_paint();
        }
    }

    pub fn set_node_bitmap(
        &mut self,
        node: NodeId,
        image: Option<Arc<ImageData>>,
    ) -> Result<(), LayoutError> {
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { name, namespace: crate::Namespace::Html, .. }) if name == "canvas" || name == "img" || name == "video")
        {
            return Err(LayoutError::InvalidTree);
        }
        if image.as_ref().is_some_and(|image| !image.is_valid()) {
            return Err(LayoutError::ImageFailed);
        }
        let previous = self
            .node_bitmaps
            .iter()
            .find(|(id, _)| *id == node)
            .map(|(_, image)| image);
        if match (previous, image.as_ref()) {
            (None, None) => true,
            (Some(previous), Some(image)) => Arc::ptr_eq(previous, image),
            _ => false,
        } {
            return Ok(());
        }
        self.node_bitmaps.retain(|(id, _)| *id != node);
        if let Some(image) = image {
            self.node_bitmaps.push((node, image));
        }
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    fn invalidate_paint(&mut self) {
        self.cached = None;
        self.layout_cache.clear();
        self.paint_revision = self.paint_revision.wrapping_add(1);
    }

    fn has_auto_directionality_controls(&mut self) -> bool {
        let document_version = self.document.version();
        if let Some((version, has_controls)) = self.auto_directionality_scan {
            if version == document_version {
                return has_controls;
            }
        }
        let has_controls = crate::directionality::has_auto_directionality_controls(&self.document);
        self.auto_directionality_scan = Some((document_version, has_controls));
        has_controls
    }

    /// Refresh retained style and paint when sparse host form or interaction
    /// state changes. Generation checks are constant-time and store no values.
    pub fn synchronize_form_state(&mut self) {
        let validity_generation = self.document.validity_generation();
        let interaction_generation = self.document.interaction_generation();
        let validity_changed = validity_generation != self.validity_generation;
        let interaction_changed = interaction_generation != self.interaction_generation;
        if !validity_changed && !interaction_changed {
            return;
        }
        self.validity_generation = validity_generation;
        self.interaction_generation = interaction_generation;
        let has_selector_dependencies = self
            .rules
            .as_ref()
            .is_some_and(StyleIndex::has_validity_data);
        if !has_selector_dependencies
            && !(validity_changed && self.has_auto_directionality_controls())
        {
            return;
        }
        self.style_cache.borrow_mut().clear();
        self.animation_snapshot = None;
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
    }
    pub fn style_cache_stats(&self) -> css::StyleCacheStats {
        self.style_cache.borrow().stats()
    }
    pub fn layout_cache_stats(&self) -> layout::RetainedLayoutCacheStats {
        self.layout_cache.stats()
    }
    /// Current environment used to evaluate stylesheet media queries.
    pub fn media_environment(&self) -> css::MediaEnvironment {
        self.style_environment.unwrap_or_default()
    }
    pub fn document(&self) -> &Document {
        &self.document
    }
    pub fn document_mut(&mut self) -> &mut Document {
        &mut self.document
    }

    /// Effective document stylesheet font descriptors in the shared cascade's
    /// layer/source priority order. Loading and choosing font resources remain host duties.
    pub fn font_faces(&mut self) -> Result<Vec<css::FontFaceRule>, LayoutError> {
        self.refresh_rules()?;
        Ok(self.font_face_rules.clone())
    }

    /// Counter that changes whenever the effective font-face rule list or any
    /// face descriptor changes; an unchanged value means `font_faces` would
    /// return an identical list for the same media environment.
    pub fn font_face_generation(&mut self) -> Result<u64, LayoutError> {
        self.refresh_rules()?;
        Ok(self.font_face_generation)
    }

    /// Renderer capability gaps in the live SVG tree and effective stylesheet
    /// graph, including linked and imported rules refreshed by this session.
    pub fn unsupported_svg_features(
        &mut self,
    ) -> Result<Vec<layout::SvgUnsupportedFeature>, LayoutError> {
        self.refresh_rules()?;
        layout::unsupported_svg_features(
            &self.document,
            self.rules.as_ref().expect("stylesheets initialized"),
        )
    }

    /// Update the descriptor snapshot for one live CSS-connected font face.
    /// The rule's owner, source, import, layer, and cascade metadata remain
    /// attached to the same identity. Manual faces are managed by the host's
    /// FontFaceSet and are not accepted here.
    ///
    /// Returns `false` when `identity` is manual or no longer belongs to a
    /// live CSS rule. A `true` result means the identity is live; paint and
    /// layout are invalidated only if the descriptor values changed.
    pub fn set_font_face_descriptors(
        &mut self,
        identity: &css::FontFaceIdentity,
        descriptors: css::FontFaceDescriptors,
    ) -> Result<bool, LayoutError> {
        self.refresh_rules()?;
        if !matches!(identity, css::FontFaceIdentity::Css(_)) {
            return Ok(false);
        }
        let Some(face_index) = self
            .font_face_rules
            .iter()
            .position(|face| face.identity.as_ref() == Some(identity))
        else {
            return Ok(false);
        };
        if self.font_face_rules[face_index].descriptors == descriptors {
            return Ok(true);
        }

        if let Some((_, current)) = self
            .font_face_descriptor_overrides
            .iter_mut()
            .find(|(current, _)| current == identity)
        {
            *current = descriptors.clone();
        } else {
            if self.font_face_descriptor_overrides.len() >= css::MAX_RULES {
                return Err(LayoutError::CommandLimit);
            }
            self.font_face_descriptor_overrides
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            self.font_face_descriptor_overrides
                .push((identity.clone(), descriptors.clone()));
        }
        self.font_face_rules[face_index].descriptors = descriptors;
        self.font_face_generation = self.font_face_generation.wrapping_add(1);
        self.invalidate_fonts();
        Ok(true)
    }

    /// Set the host's effective document base for inline CSS URL references.
    pub fn set_document_base_url(&mut self, base: Option<Arc<str>>) {
        if self.document_base_url == base {
            return;
        }
        self.document_base_url = base;
        self.rules = None;
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
    }

    /// Attach a fetched import graph to its original stylesheet owner. The
    /// owner's DOM text and attributes remain unchanged; later href/text
    /// mutations hide this snapshot until the host supplies a fresh graph.
    pub fn set_stylesheet_source(
        &mut self,
        node: NodeId,
        source: Option<css::StylesheetSource>,
    ) -> Result<(), LayoutError> {
        let identity =
            layout::stylesheet_identity(&self.document, node)?.ok_or(LayoutError::InvalidTree)?;
        if let Some(source) = &source {
            if let layout::StylesheetIdentity::Inline(text) = &identity {
                if source.text.as_ref() != text {
                    return Err(LayoutError::InvalidTree);
                }
            }
            css::parse_graph(source, self.media_environment()).map_err(LayoutError::Css)?;
        }
        let previous_sources = self.stylesheet_sources.clone();
        let previous_linked = self.linked_stylesheets.clone();
        self.stylesheet_sources
            .retain(|loaded| loaded.owner != node);
        if let layout::StylesheetIdentity::Link(href) = &identity {
            self.linked_stylesheets
                .retain(|(owner, _, _)| *owner != node);
            if let Some(source) = &source {
                self.linked_stylesheets.push((
                    node,
                    href.clone(),
                    String::from(source.text.as_ref()),
                ));
            }
        }
        if let Some(source) = source {
            self.stylesheet_sources.push(layout::LoadedStylesheet {
                owner: node,
                identity,
                source,
            });
        }
        if self.stylesheet_sources == previous_sources && self.linked_stylesheets == previous_linked
        {
            return Ok(());
        }
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            self.stylesheet_sources = previous_sources;
            self.linked_stylesheets = previous_linked;
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    pub fn stylesheet_source(&self, node: NodeId) -> Option<&css::StylesheetSource> {
        let identity = layout::stylesheet_identity(&self.document, node).ok()??;
        self.stylesheet_sources
            .iter()
            .find(|source| source.owner == node && source.identity == identity)
            .filter(|source| match &source.identity {
                layout::StylesheetIdentity::Link(_) => {
                    self.linked_stylesheet(node) == Some(source.source.text.as_ref())
                }
                layout::StylesheetIdentity::Inline(_) => true,
            })
            .map(|source| &source.source)
    }

    /// Loaded child sheet for one `@import` URL, if that import succeeded.
    pub fn imported_stylesheet_source(&self, node: NodeId, href: &str) -> Option<String> {
        let source = self.stylesheet_source(node)?;
        source
            .imports
            .iter()
            .find(|import| import.rule.url.as_ref() == href)
            .and_then(|import| import.source.as_deref())
            .map(|source| String::from(source.text.as_ref()))
    }

    pub fn imported_child_stylesheet(
        &self,
        node: NodeId,
        path: &[usize],
        href: &str,
    ) -> Option<(usize, String, String)> {
        let mut source = self.stylesheet_source(node)?;
        for &index in path {
            source = source.imports.get(index)?.source.as_deref()?;
        }
        source
            .imports
            .iter()
            .enumerate()
            .find(|(_, import)| import.rule.url.as_ref() == href)
            .and_then(|(index, import)| {
                import.source.as_deref().map(|source| {
                    (
                        index,
                        String::from(source.text.as_ref()),
                        String::from(source.url.as_ref()),
                    )
                })
            })
    }

    pub fn imported_stylesheet_text(&self, node: NodeId, path: &[usize]) -> Option<String> {
        let mut source = self.stylesheet_source(node)?;
        for &index in path {
            source = source.imports.get(index)?.source.as_deref()?;
        }
        Some(String::from(source.text.as_ref()))
    }

    pub fn replace_imported_stylesheet_text(
        &mut self,
        node: NodeId,
        path: &[usize],
        text: &str,
    ) -> Result<(), LayoutError> {
        let previous = self.stylesheet_sources.clone();
        let loaded = self
            .stylesheet_sources
            .iter_mut()
            .find(|source| source.owner == node)
            .ok_or(LayoutError::InvalidTree)?;
        let mut child = &mut loaded.source;
        for &index in path {
            child = child
                .imports
                .get_mut(index)
                .and_then(|import| import.source.as_deref_mut())
                .ok_or(LayoutError::InvalidTree)?;
        }
        child.text = Arc::from(text);
        if let Some(loaded) = self
            .stylesheet_sources
            .iter()
            .find(|source| source.owner == node)
        {
            if let Err(error) = css::parse_graph(&loaded.source, self.media_environment()) {
                self.stylesheet_sources = previous;
                return Err(LayoutError::Css(error));
            }
        }
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            self.stylesheet_sources = previous;
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Selects a connected element to render in the viewport's presentation
    /// layer. `None` restores document flow. The selected element must belong
    /// to this session's document; detached or stale IDs are rejected.
    pub fn set_presentation_root(&mut self, root: Option<NodeId>) -> Result<(), LayoutError> {
        if let Some(root) = root {
            let mut ancestor = Some(root);
            let mut connected = false;
            while let Some(node) = ancestor {
                if node == self.document.root() {
                    connected = true;
                    break;
                }
                ancestor = self
                    .document
                    .parent(node)
                    .map_err(|_| LayoutError::InvalidTree)?;
            }
            if !matches!(self.document.kind(root), Ok(NodeKind::Element { .. })) || !connected {
                return Err(LayoutError::InvalidTree);
            }
        }
        if self.presentation_root != root {
            self.presentation_root = root;
            self.cached = None;
            self.layout_cache.clear();
            self.paint_revision = self.paint_revision.wrapping_add(1);
        }
        Ok(())
    }

    pub fn presentation_root(&self) -> Option<NodeId> {
        self.presentation_root
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
        self.style_environment = Some(environment);
        self.invalidate_paint();
        self.frame_id += 1;
        Ok(())
    }

    /// Return the resource text belonging to a link's current href. Resource
    /// loading is a host responsibility; changing href hides the stale sheet.
    pub fn linked_stylesheet(&self, node: NodeId) -> Option<&str> {
        let NodeKind::Element {
            name,
            namespace: crate::Namespace::Html,
            attributes,
        } = self.document.kind(node).ok()?
        else {
            return None;
        };
        if name != "link" {
            return None;
        }
        let href = attributes
            .iter()
            .find(|(name, _)| name == "href")?
            .1
            .as_str();
        self.linked_stylesheets
            .iter()
            .find(|(owner, loaded_href, _)| *owner == node && loaded_href == href)
            .map(|(_, _, text)| text.as_str())
    }

    /// Attach or unload a fetched stylesheet without changing its DOM owner.
    /// Sheets participate at the link's position in document order; detached,
    /// disabled and media-mismatched links do not affect the live cascade.
    pub fn set_linked_stylesheet(
        &mut self,
        node: NodeId,
        text: Option<String>,
    ) -> Result<(), LayoutError> {
        let NodeKind::Element {
            name,
            namespace: crate::Namespace::Html,
            attributes,
        } = self
            .document
            .kind(node)
            .map_err(|_| LayoutError::InvalidTree)?
        else {
            return Err(LayoutError::InvalidTree);
        };
        if name != "link" {
            return Err(LayoutError::InvalidTree);
        }
        let href = attributes
            .iter()
            .find(|(name, _)| name == "href")
            .map(|(_, value)| value.clone())
            .unwrap_or_default();
        if let Some(text) = &text {
            css::parse(text).map_err(LayoutError::Css)?;
        }
        let previous = self.linked_stylesheets.clone();
        self.linked_stylesheets
            .retain(|(owner, _, _)| *owner != node);
        if let Some(text) = text {
            self.linked_stylesheets.push((node, href, text));
        }
        if previous == self.linked_stylesheets {
            return Ok(());
        }
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            self.linked_stylesheets = previous;
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.invalidate_paint();
        self.frame_id += 1;
        Ok(())
    }

    /// Replace document-adopted constructable stylesheet sources. They are
    /// parsed with the shared CSS parser and appended after DOM stylesheet
    /// rules in adoption order. Invalid input leaves the prior sheet set intact.
    pub fn set_adopted_stylesheets(
        &mut self,
        scope: Option<NodeId>,
        stylesheets: Vec<String>,
    ) -> Result<(), LayoutError> {
        for stylesheet in &stylesheets {
            css::parse_scoped(stylesheet, scope).map_err(LayoutError::Css)?;
        }
        let Some(index) = self
            .adopted_stylesheets
            .iter()
            .position(|(candidate, _)| *candidate == scope)
        else {
            if stylesheets.is_empty() {
                return Ok(());
            }
            self.adopted_stylesheets.push((scope, stylesheets));
            self.rules = None;
            if let Err(error) = self.refresh_rules() {
                self.adopted_stylesheets.pop();
                self.rules = None;
                let _ = self.refresh_rules();
                return Err(error);
            }
            self.invalidate_paint();
            self.frame_id += 1;
            return Ok(());
        };
        if self.adopted_stylesheets[index].1 == stylesheets {
            return Ok(());
        }
        let previous = core::mem::replace(&mut self.adopted_stylesheets[index].1, stylesheets);
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            self.adopted_stylesheets[index].1 = previous;
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.invalidate_paint();
        self.frame_id += 1;
        Ok(())
    }

    /// Update one node's Web Animations declarations without mutating its DOM
    /// attributes or producing mutation-observer records.
    pub fn set_animation_declarations(
        &mut self,
        node: NodeId,
        declarations: Vec<(String, String)>,
    ) -> Result<(), LayoutError> {
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules_only()?;
        let position = self
            .animated_styles
            .iter()
            .position(|(target, _)| *target == node);
        let paint_only = {
            let previous: &[(String, String)] =
                position.map_or(&[][..], |index| self.animated_styles[index].1.as_slice());
            if previous == declarations.as_slice() {
                return Ok(());
            }
            changed_declarations_paint_only(previous, &declarations)
        };
        match position {
            Some(index) => self.animated_styles[index].1 = declarations.clone(),
            None => self.animated_styles.push((node, declarations.clone())),
        }
        self.rules
            .as_mut()
            .expect("stylesheets initialized")
            .set_animation_declarations(node, &declarations)
            .map_err(LayoutError::Css)?;
        self.animation_epoch = self.animation_epoch.wrapping_add(1);
        if paint_only {
            if !self.pending_animation_full && self.pending_animation_nodes.last() != Some(&node) {
                self.pending_animation_nodes.push(node);
            }
        } else {
            self.pending_animation_full = true;
            self.pending_animation_nodes.clear();
        }
        self.cached = None;
        self.frame_id += 1;
        Ok(())
    }

    /// Apply animation declaration changes accumulated since the last
    /// rendering opportunity as one invalidation of the widest class seen.
    fn flush_animation_changes(&mut self) {
        if self.pending_animation_full {
            self.pending_animation_full = false;
            self.style_cache.borrow_mut().clear();
            self.invalidate_paint();
            return;
        }
        if self.pending_animation_nodes.is_empty() {
            return;
        }
        let mut nodes = core::mem::take(&mut self.pending_animation_nodes);
        nodes.sort_by_key(|node| node.index());
        nodes.dedup();
        if nodes
            .iter()
            .any(|node| self.document.kind(*node).is_err())
        {
            self.style_cache.borrow_mut().clear();
            self.invalidate_paint();
            return;
        }
        {
            let mut cache = self.style_cache.borrow_mut();
            for node in &nodes {
                cache.invalidate_node(*node);
            }
        }
        self.cached = None;
        self.layout_cache
            .invalidate_subtree_targets(&self.document, &nodes);
        self.paint_revision = self.paint_revision.wrapping_add(1);
    }

    pub fn computed_style(&mut self, node: NodeId) -> Result<Style, LayoutError> {
        self.synchronize_form_state();
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules()?;
        let rules = self.rules.as_ref().expect("stylesheets initialized");
        let key = ReadStyleKey {
            document_version: self.document.version(),
            rules_generation: self.rules_generation,
            validity_generation: self.validity_generation,
            interaction_generation: self.interaction_generation,
            animation_epoch: self.animation_epoch,
            environment: rules.environment,
        };
        if self.read_style_key != Some(key) {
            self.read_style_cache = css::StyleCache::default();
            self.read_style_key = Some(key);
        }
        let cache = &mut self.read_style_cache;
        let mut ancestors = Vec::new();
        let mut current = Some(node);
        let mut parent_style = None;
        while let Some(id) = current {
            if let Some(style) = cache.node_style(id) {
                parent_style = Some(style);
                break;
            }
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
                .composed_parent(id)
                .map_err(|_| LayoutError::InvalidTree)?;
        }
        for id in ancestors.into_iter().rev() {
            let style = css::compute_node_cached(
                &self.document,
                id,
                parent_style.as_deref(),
                rules,
                cache,
            )
            .map_err(LayoutError::Css)?;
            parent_style = Some(cache.remember_snapshot_style(&self.document, id, style));
        }
        parent_style
            .map(|style| (*style).clone())
            .ok_or(LayoutError::InvalidTree)
    }

    /// Snapshot the computed CSS animation inputs and retained keyframes for
    /// the host animation adapter. Traversal covers document and shadow trees.
    pub fn animation_snapshot(&mut self) -> Result<Arc<AnimationSnapshot>, LayoutError> {
        self.synchronize_form_state();
        self.refresh_rules()?;
        let version = self.document.version();
        let rules = self.rules.as_ref().expect("stylesheets initialized");
        if let Some(snapshot) = self.animation_snapshot.as_ref().filter(|snapshot| {
            snapshot.document_version == version && snapshot.environment == rules.environment
        }) {
            return Ok(snapshot.clone());
        }
        // This cache belongs to this immutable snapshot build. Reuse ancestor
        // styles while walking both light and shadow trees; each node cascades
        // at most once, using its actual composed parent for inheritance.
        let mut cache = css::StyleCache::default();
        let mut roots = alloc::vec![self.document.root()];
        roots.extend(
            self.document
                .shadow_roots()
                .into_iter()
                .map(|(_, root, _)| root),
        );
        let mut nodes = Vec::new();
        while let Some(node) = roots.pop() {
            if matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
                let mut ancestors = Vec::new();
                let mut current = Some(node);
                let mut parent_style = None;
                while let Some(id) = current {
                    if let Some(style) = cache.node_style(id) {
                        parent_style = Some(style);
                        break;
                    }
                    if ancestors.len() >= 512 {
                        return Err(LayoutError::DepthLimit);
                    }
                    ancestors.push(id);
                    current = self
                        .document
                        .composed_parent(id)
                        .map_err(|_| LayoutError::InvalidTree)?;
                }
                for id in ancestors.into_iter().rev() {
                    if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
                        let style = css::compute_node_cached(
                            &self.document,
                            id,
                            parent_style.as_deref(),
                            rules,
                            &mut cache,
                        )
                        .map_err(LayoutError::Css)?;
                        parent_style =
                            Some(cache.remember_snapshot_style(&self.document, id, style));
                    }
                }
                let style = parent_style.ok_or(LayoutError::InvalidTree)?;
                let mut rendered = !matches!(style.display, css::Display::None);
                if rendered && style.animation.iter().any(Option::is_some) {
                    let mut ancestor = self
                        .document
                        .composed_parent(node)
                        .map_err(|_| LayoutError::InvalidTree)?;
                    while let Some(id) = ancestor {
                        if cache
                            .node_style(id)
                            .is_some_and(|style| matches!(style.display, css::Display::None))
                        {
                            rendered = false;
                            break;
                        }
                        ancestor = self
                            .document
                            .composed_parent(id)
                            .map_err(|_| LayoutError::InvalidTree)?;
                    }
                }
                if rendered && style.animation.iter().any(Option::is_some) {
                    let root = self
                        .document
                        .root_node(node, false)
                        .map_err(|_| LayoutError::InvalidTree)?;
                    let scope = (root != self.document.root()).then_some(root);
                    nodes.push((node, scope, style.animation.clone()));
                }
            }
            let mut child = self
                .document
                .first_child(node)
                .map_err(|_| LayoutError::InvalidTree)?;
            while let Some(id) = child {
                roots.push(id);
                child = self
                    .document
                    .next_sibling(id)
                    .map_err(|_| LayoutError::InvalidTree)?;
            }
        }
        self.animation_generation = self.animation_generation.wrapping_add(1);
        let snapshot = Arc::new(AnimationSnapshot {
            generation: self.animation_generation,
            nodes,
            keyframes: rules.keyframes.clone(),
            document_version: version,
            environment: rules.environment,
        });
        self.animation_snapshot = Some(snapshot.clone());
        Ok(snapshot)
    }

    fn refresh_rules(&mut self) -> Result<(), LayoutError> {
        self.refresh_rules_only()?;
        self.flush_animation_changes();
        Ok(())
    }

    fn refresh_rules_only(&mut self) -> Result<(), LayoutError> {
        let version = self.document.version();
        self.animated_styles
            .retain(|(node, _)| self.document.kind(*node).is_ok());
        self.stylesheet_sources
            .retain(|source| self.document.kind(source.owner).is_ok());
        self.linked_stylesheets
            .retain(|(node, _, _)| self.document.kind(*node).is_ok());
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
                        MutationKind::Attribute(_) => in_style(&self.document, mutation.target),
                    });
            if rules_changed {
                self.animation_snapshot = None;
                let environment = self
                    .rules
                    .as_ref()
                    .map(|rules| rules.environment)
                    .or(self.style_environment)
                    .unwrap_or_default();
                let (mut rules, mut font_faces) = layout::stylesheets_with_sources(
                    &self.document,
                    &self.linked_stylesheets,
                    &self.stylesheet_sources,
                    &self.adopted_stylesheets,
                    self.document_base_url.clone(),
                    environment,
                )?;
                rules.environment = environment;
                self.animated_styles
                    .retain(|(node, _)| self.document.kind(*node).is_ok());
                for (node, declarations) in &self.animated_styles {
                    rules
                        .set_animation_declarations(*node, declarations)
                        .map_err(LayoutError::Css)?;
                }
                self.font_face_descriptor_overrides.retain(|(identity, _)| {
                    font_faces
                        .iter()
                        .any(|face| face.identity.as_ref() == Some(identity))
                });
                for face in &mut font_faces {
                    if let Some(identity) = &face.identity {
                        if let Some((_, descriptors)) = self
                            .font_face_descriptor_overrides
                            .iter()
                            .find(|(current, _)| current == identity)
                        {
                            face.descriptors = descriptors.clone();
                        }
                    }
                }
                self.rules = Some(rules);
                self.rules_generation = self.rules_generation.wrapping_add(1);
                self.font_face_rules = font_faces;
                self.font_face_generation = self.font_face_generation.wrapping_add(1);
                self.style_cache.borrow_mut().clear();
            }
            self.rules_version = version;
        }
        Ok(())
    }
    pub fn invalidate_fonts(&mut self) {
        self.cached = None;
        self.style_cache.borrow_mut().clear();
        self.layout_cache.invalidate_fonts();
        self.paint_revision = self.paint_revision.wrapping_add(1);
        self.frame_id += 1;
    }

    /// Invalidate retained layout and paint after a host image resource changes.
    pub fn invalidate_image_resources(&mut self) {
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
    }

    /// Hit-test the last completed layout in CSS pixels. Mutated documents need a new frame.
    pub fn hit_test(&self, x_css: f32, y_css: f32) -> Option<NodeId> {
        let mut result = None;
        self.for_each_hit_test(x_css, y_css, |node| {
            result = Some(node);
            false
        });
        result
    }

    /// Visit hit regions from topmost to bottommost without allocating a list.
    /// Returning false from the visitor stops the walk. The return value says
    /// whether the point belongs to a current completed viewport, including a
    /// valid point with no hit regions. Fragmented elements may occur more than
    /// once; consumers exposing element sequences resolve those identities.
    pub fn for_each_hit_test(
        &self,
        x_css: f32,
        y_css: f32,
        mut visit: impl FnMut(NodeId) -> bool,
    ) -> bool {
        let Some(frame) = self.cached.as_ref() else {
            return false;
        };
        if frame.document_version != self.document.version()
            || !x_css.is_finite()
            || !y_css.is_finite()
            || x_css < 0.0
            || y_css < 0.0
            || x_css >= frame.width as f32
            || y_css >= frame.height as f32
        {
            return false;
        }
        for (index, hit) in frame.geometry.hits.iter().enumerate().rev() {
            if frame.geometry.contains_hit(index, x_css, y_css) && !visit(hit.node) {
                break;
            }
        }
        true
    }

    /// Return the visible border-box bounds for a node in the last completed
    /// CSS-pixel layout. Nodes without a fresh, composed hit region have no
    /// bounds. Transforms and overflow clips are checked, while returned bounds
    /// retain the unclipped border-box origin for content-box calculations.
    pub fn hit_bounds(&self, node: NodeId) -> Option<Rect> {
        let frame = self.cached.as_ref()?;
        if frame.document_version != self.document.version() || self.document.kind(node).is_err() {
            return None;
        }
        let mut result: Option<Rect> = None;
        for (index, hit) in frame.geometry.hits.iter().enumerate() {
            if hit.node != node || hit.virtual_generated {
                continue;
            }
            let mut visible = true;
            let transforms = frame
                .geometry
                .transforms
                .iter()
                .filter(|transform| transform.hits.contains(&index))
                .map(|transform| transform.matrix);
            let Some(mut bounds) = layout::transformed_bounds(hit.rect, transforms) else {
                continue;
            };
            for clip in frame
                .geometry
                .rounded_clips
                .iter()
                .filter(|clip| clip.hits.contains(&index))
            {
                let transforms = frame.geometry.transforms[clip.first_transform..]
                    .iter()
                    .filter(|transform| transform.hits.contains(&index))
                    .map(|transform| transform.matrix);
                let Some(clip_bounds) = layout::transformed_bounds(clip.rect, transforms) else {
                    visible = false;
                    break;
                };
                if bounds.intersection(clip_bounds).is_none() {
                    visible = false;
                    break;
                }
            }
            if !visible {
                continue;
            }
            result = Some(match result {
                Some(current) => {
                    let left = current.x.min(bounds.x);
                    let top = current.y.min(bounds.y);
                    let right = (current.x + current.width).max(bounds.x + bounds.width);
                    let bottom = (current.y + current.height).max(bounds.y + bounds.height);
                    Rect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top,
                    }
                }
                None => bounds,
            });
        }
        result.filter(|bounds| {
            bounds
                .intersection(Rect {
                    x: 0.0,
                    y: 0.0,
                    width: frame.width as f32,
                    height: frame.height as f32,
                })
                .is_some()
        })
    }

    /// Return the visible retained border-box bounds for a node before its CSS
    /// transform is applied. Scroll offsets are already reflected in this
    /// geometry. This is intended for consumers that must inset the box before
    /// applying the same retained transform, such as native text overlays.
    pub fn untransformed_hit_bounds(&self, node: NodeId) -> Option<Rect> {
        let frame = self.cached.as_ref()?;
        if frame.document_version != self.document.version() || self.document.kind(node).is_err() {
            return None;
        }
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: frame.width as f32,
            height: frame.height as f32,
        };
        let mut result: Option<Rect> = None;
        for (index, hit) in frame.geometry.hits.iter().enumerate() {
            if hit.node != node || hit.virtual_generated {
                continue;
            }
            let transforms = frame
                .geometry
                .transforms
                .iter()
                .filter(|transform| transform.hits.contains(&index))
                .map(|transform| transform.matrix);
            let Some(visible_bounds) = layout::transformed_bounds(hit.rect, transforms) else {
                continue;
            };
            if visible_bounds.intersection(viewport).is_none() {
                continue;
            }
            let mut visible = true;
            for clip in frame
                .geometry
                .rounded_clips
                .iter()
                .filter(|clip| clip.hits.contains(&index))
            {
                let transforms = frame.geometry.transforms[clip.first_transform..]
                    .iter()
                    .filter(|transform| transform.hits.contains(&index))
                    .map(|transform| transform.matrix);
                let Some(clip_bounds) = layout::transformed_bounds(clip.rect, transforms) else {
                    visible = false;
                    break;
                };
                if visible_bounds.intersection(clip_bounds).is_none() {
                    visible = false;
                    break;
                }
            }
            if !visible {
                continue;
            }
            result = Some(match result {
                Some(current) => {
                    let left = current.x.min(hit.rect.x);
                    let top = current.y.min(hit.rect.y);
                    let right = (current.x + current.width).max(hit.rect.x + hit.rect.width);
                    let bottom = (current.y + current.height).max(hit.rect.y + hit.rect.height);
                    Rect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top,
                    }
                }
                None => hit.rect,
            });
        }
        result
    }

    /// Return each rendered border-box fragment for `node` in viewport CSS
    /// pixels. The values come from the last completed layout and become
    /// unavailable as soon as the document changes.
    pub fn client_rects(&self, node: NodeId) -> Vec<Rect> {
        self.client_rect_iter(node).collect()
    }

    fn client_rect_iter(&self, node: NodeId) -> impl Iterator<Item = Rect> + '_ {
        self.cached
            .as_ref()
            .filter(|frame| {
                frame.document_version == self.document.version()
                    && self.document.kind(node).is_ok()
            })
            .into_iter()
            .flat_map(move |frame| {
                frame
                    .geometry
                    .hits
                    .iter()
                    .enumerate()
                    .filter_map(move |(index, hit)| {
                        if hit.node != node || hit.virtual_generated || !hit.rect.is_valid() {
                            return None;
                        }
                        let transforms = frame
                            .geometry
                            .transforms
                            .iter()
                            .filter(|transform| transform.hits.contains(&index))
                            .map(|transform| transform.matrix);
                        layout::transformed_bounds(hit.rect, transforms)
                    })
            })
    }

    /// Union transformed rendered fragments without allocating a fragment list.
    /// Return the first fragment when every fragment has a zero dimension;
    /// otherwise include each fragment with at least one nonzero dimension.
    pub fn bounding_client_rect(&self, node: NodeId) -> Option<Rect> {
        Self::client_rect_bounds(self.client_rect_iter(node))
    }

    fn client_rect_bounds(mut rects: impl Iterator<Item = Rect>) -> Option<Rect> {
        let first = rects.next()?;
        let mut bounds = None;
        let mut has_full_box = false;
        for rect in core::iter::once(first).chain(rects) {
            has_full_box |= rect.width != 0.0 && rect.height != 0.0;
            if rect.width == 0.0 && rect.height == 0.0 {
                continue;
            }
            bounds = Some(match bounds {
                None => rect,
                Some(current) => {
                    let current: Rect = current;
                    let x = current.x.min(rect.x);
                    let y = current.y.min(rect.y);
                    Rect {
                        x,
                        y,
                        width: (current.x + current.width).max(rect.x + rect.width) - x,
                        height: (current.y + current.height).max(rect.y + rect.height) - y,
                    }
                }
            });
        }
        Some(if has_full_box {
            bounds.unwrap_or(first)
        } else {
            first
        })
    }

    /// Untransformed border-box bounds from the current frame. This is useful
    /// for offset geometry, whose layout offsets are independent of CSS
    /// transforms.
    pub fn layout_rect(&self, node: NodeId) -> Option<Rect> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        let mut rects = frame
            .geometry
            .hits
            .iter()
            .filter(|hit| hit.node == node && !hit.virtual_generated && hit.rect.is_valid())
            .map(|hit| hit.rect);
        let first = rects.next()?;
        Some(rects.fold(first, |current, rect| {
            let left = current.x.min(rect.x);
            let top = current.y.min(rect.y);
            let right = (current.x + current.width).max(rect.x + rect.width);
            let bottom = (current.y + current.height).max(rect.y + rect.height);
            Rect {
                x: left,
                y: top,
                width: right - left,
                height: bottom - top,
            }
        }))
    }

    /// Bounds of the element's rendered border-box fragments after retained
    /// overflow clipping and the viewport clip have been applied.
    pub fn visible_bounds(&self, node: NodeId) -> Option<Rect> {
        self.clipped_bounds(node, true)
    }

    /// Return the retained overflow clip chain for a node. The snapshot is
    /// valid only for the current cached document version, like other geometry
    /// queries on this session.
    pub fn retained_overflow_clips(&self, node: NodeId) -> Vec<RetainedOverflowClip> {
        let Some(frame) = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        }) else {
            return Vec::new();
        };
        let mut clips = Vec::new();
        for clip in &frame.geometry.rounded_clips {
            let Some(hit_index) = clip.hits.clone().find(|index| {
                frame
                    .geometry
                    .hits
                    .get(*index)
                    .is_some_and(|hit| hit.node == node && !hit.virtual_generated)
            }) else {
                continue;
            };
            let mut transforms = Vec::new();
            for transform in &frame.geometry.transforms[clip.first_transform..] {
                if transform.hits.contains(&hit_index) {
                    transforms.push(transform.matrix);
                }
            }
            clips.push(RetainedOverflowClip {
                rect: clip.rect,
                corners: clip.corners.as_deref().copied(),
                radius: clip.radius,
                transforms,
            });
        }
        clips
    }

    /// Bounds after retained overflow clips but before viewport clipping.
    /// IntersectionObserver applies the root rectangle after `rootMargin`,
    /// which may legitimately extend beyond the viewport.
    pub fn overflow_clipped_bounds(&self, node: NodeId) -> Option<Rect> {
        self.clipped_bounds(node, false)
    }

    fn clipped_bounds(&self, node: NodeId, clip_viewport: bool) -> Option<Rect> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: frame.width as f32,
            height: frame.height as f32,
        };
        let mut result: Option<Rect> = None;
        for (index, hit) in frame.geometry.hits.iter().enumerate() {
            if hit.node != node || hit.virtual_generated {
                continue;
            }
            let transforms = frame
                .geometry
                .transforms
                .iter()
                .filter(|transform| transform.hits.contains(&index))
                .map(|transform| transform.matrix);
            let Some(mut visible) = layout::transformed_bounds(hit.rect, transforms) else {
                continue;
            };
            for clip in frame
                .geometry
                .rounded_clips
                .iter()
                .filter(|clip| clip.hits.contains(&index))
            {
                let transforms = frame.geometry.transforms[clip.first_transform..]
                    .iter()
                    .filter(|transform| transform.hits.contains(&index))
                    .map(|transform| transform.matrix);
                let Some(clip_bounds) = layout::transformed_bounds(clip.rect, transforms) else {
                    visible = Rect {
                        x: visible.x,
                        y: visible.y,
                        width: 0.0,
                        height: 0.0,
                    };
                    break;
                };
                let Some(intersection) = visible.intersection(clip_bounds) else {
                    visible = Rect {
                        x: visible.x,
                        y: visible.y,
                        width: 0.0,
                        height: 0.0,
                    };
                    break;
                };
                visible = intersection;
            }
            if clip_viewport {
                let Some(clipped) = visible.intersection(viewport) else {
                    continue;
                };
                visible = clipped;
            }
            result = Some(match result {
                Some(current) => {
                    let left = current.x.min(visible.x);
                    let top = current.y.min(visible.y);
                    let right = (current.x + current.width).max(visible.x + visible.width);
                    let bottom = (current.y + current.height).max(visible.y + visible.height);
                    Rect {
                        x: left,
                        y: top,
                        width: right - left,
                        height: bottom - top,
                    }
                }
                None => visible,
            });
        }
        result
    }

    /// Scrollable overflow extent recorded by layout, in CSS pixels. This is
    /// `None` until a fresh layout has completed or when the node is not a
    /// scroll container.
    pub fn scroll_extent(&self, node: NodeId) -> Option<(f32, f32)> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        frame
            .geometry
            .scroll_extents
            .iter()
            .find(|extent| extent.node == node)
            .map(|extent| (extent.x, extent.y))
    }

    /// The padding-box scrollport in viewport CSS pixels from the last fresh
    /// layout. The root viewport has no owner hit; element scrollports use the
    /// owner's retained transform chain without allocating a hit-list copy.
    pub fn scrollport(&self, node: NodeId) -> Option<Rect> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        let port = frame
            .geometry
            .scroll_ports
            .iter()
            .find(|port| port.node == node)?;
        let Some(owner_hit) = port.owner_hit else {
            return port.rect.is_valid().then_some(port.rect);
        };
        frame.geometry.hits.get(owner_hit)?;
        let transforms = frame
            .geometry
            .transforms
            .iter()
            .filter(|transform| transform.hits.contains(&owner_hit))
            .map(|transform| transform.matrix);
        layout::transformed_bounds(port.rect, transforms)
    }

    /// Whether this element is positioned against the viewport in the most
    /// recent fresh layout. Callers walking composed ancestors can stop before
    /// scrolling the viewport when they encounter such a fixed root.
    pub fn is_viewport_fixed(&self, node: NodeId) -> bool {
        self.cached.as_ref().is_some_and(|frame| {
            frame.document_version == self.document.version()
                && self.document.kind(node).is_ok()
                && frame.geometry.viewport_fixed_nodes.contains(&node)
        })
    }

    /// CSS-pixel viewport of the last successful layout, when still fresh.
    pub fn viewport_size(&self) -> Option<(u32, u32)> {
        let frame = self
            .cached
            .as_ref()
            .filter(|frame| frame.document_version == self.document.version())?;
        Some((frame.width, frame.height))
    }

    /// The current resolved text runs for a form control, in visual line
    /// order. `range` addresses UTF-8 bytes in the original control value;
    /// `display_range` addresses the shaped text and can differ for masked
    /// password text or placeholder content. Run coordinates include scroll
    /// offsets, and `transform` maps them to viewport coordinates.
    pub fn control_text_runs(&self, node: NodeId) -> impl Iterator<Item = &layout::ControlTextRun> {
        self.cached
            .as_ref()
            .filter(|frame| frame.document_version == self.document.version())
            .into_iter()
            .flat_map(move |frame| {
                frame
                    .geometry
                    .control_text_runs
                    .iter()
                    .filter(move |run| run.node == node)
            })
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
        let extent = self.cached.as_ref().and_then(|frame| {
            frame
                .geometry
                .scroll_extents
                .iter()
                .find(|extent| extent.node == node)
                .map(|extent| (extent.x, extent.y))
        });
        // The viewport records no extent when its content fits, but a stale offset
        // from earlier, larger content must still be clamped back to zero.
        let Some((max_x, max_y)) = extent.or_else(|| {
            (node == self.document.root() && self.cached.is_some()).then_some((0.0, 0.0))
        }) else {
            return Ok(false);
        };
        let (x, y) = (x_css.clamp(0.0, max_x), y_css.clamp(0.0, max_y));
        let from = self.scroll_offset(node);
        if from == (x, y) {
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
        let replayed = self.cached.as_mut().is_some_and(|frame| {
            frame
                .geometry
                .scroll_region(&mut frame.list.0, node, from, (x, y))
        });
        if !replayed {
            self.cached = None;
        }
        self.layout_cache.clear();
        self.paint_revision = self.paint_revision.wrapping_add(1);
        self.frame_id += 1;
        Ok(true)
    }

    /// The list produced by the most recent successful render, if still valid.
    pub fn cached_display_list(&self) -> Option<&DisplayList> {
        self.cached
            .as_ref()
            .filter(|frame| frame.document_version == self.document.version())
            .map(|frame| &frame.list)
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
        self.synchronize_form_state();
        let _html_allocations =
            lumen_common::memcat::enter(lumen_common::memcat::CategoryTag::HTML);
        self.flush_animation_changes();
        let font_generation = text.generation();
        if self.font_generation != font_generation {
            self.invalidate_fonts();
            self.font_generation = font_generation;
        }
        self.scrolls
            .retain(|scroll| self.document.kind(scroll.node).is_ok());
        self.node_bitmaps
            .retain(|(node, _)| self.document.kind(*node).is_ok());
        let version = self.document.version();
        let image_generation = images.map(ImageResolver::generation);
        if self.cached.as_ref().is_some_and(|cached| {
            cached.width != width
                || cached.height != height
                || cached.image_generation != image_generation
        }) {
            self.layout_cache.clear();
        }
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
            let rules = self.rules.as_ref().expect("stylesheets initialized");
            let only_text_mutations = !self.document.mutations().is_empty()
                && self.document.mutations().iter().all(|mutation| {
                    matches!(mutation.kind, MutationKind::CharacterData)
                        && !in_style(&self.document, mutation.target)
                });
            let only_local_mutations = !self.document.mutations().is_empty()
                && self.document.mutations().iter().all(|mutation| {
                    !in_style(&self.document, mutation.target)
                        && match &mutation.kind {
                            MutationKind::CharacterData | MutationKind::Attribute(_) => true,
                            MutationKind::Tree {
                                added,
                                removed,
                                styles_changed,
                            } => {
                                !*styles_changed
                                    && !added
                                        .is_some_and(|node| subtree_has_style(&self.document, node))
                                    && !removed
                                        .is_some_and(|node| subtree_has_style(&self.document, node))
                            }
                            MutationKind::FullRebuild => false,
                        }
                });
            // Counter and quote output depends on preceding siblings, not just
            // the changed subtree. Keep reuse on unchanged frames, but recapture
            // fragments after mutations in documents with that shared context.
            let generated_context_changed = self.style_version != version
                && (rules.has_counter_data()
                    || rules.has_quote_content()
                    || rules.has_list_item_data()
                    || layout::contains_list_item_candidate(&self.document, self.document.root())?);
            let can_retain = !generated_context_changed
                && (self.style_version == version
                    || (only_local_mutations && rules.siblings_share));
            let environment_changed = self.style_environment != Some(rules.environment);
            if !can_retain || environment_changed {
                self.style_cache.borrow_mut().clear();
                self.layout_cache.clear();
            } else if self.style_version != version {
                let mut targets: Vec<_> = self
                    .document
                    .mutations()
                    .iter()
                    .map(|mutation| mutation.target)
                    .collect();
                let slot_hosts: Vec<_> = self
                    .document
                    .mutations()
                    .iter()
                    .filter_map(|mutation| match &mutation.kind {
                        MutationKind::Attribute(name) => {
                            slot_assignment_host(&self.document, mutation.target, name.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                targets.extend(slot_hosts.iter().copied());
                if only_text_mutations {
                    self.layout_cache
                        .invalidate_text_targets(&self.document, &targets);
                } else {
                    // Attribute changes and moved subtrees can change ancestry
                    // matches even if the parent's computed style is identical.
                    self.style_cache.borrow_mut().clear();
                    self.layout_cache
                        .invalidate_text_targets(&self.document, &targets);
                    let mut subtrees = Vec::new();
                    for mutation in self.document.mutations() {
                        match &mutation.kind {
                            MutationKind::Attribute(_) => subtrees.push(mutation.target),
                            MutationKind::Tree { added, removed, .. } => {
                                subtrees.extend(added.iter().copied());
                                subtrees.extend(removed.iter().copied());
                            }
                            _ => {}
                        }
                    }
                    // Slot/name mutations can move assigned light children
                    // between sibling slots. The changed node's new composed
                    // ancestry does not include the slot fragment that lost
                    // those children, so invalidate their common host subtree.
                    subtrees.extend(slot_hosts);
                    self.layout_cache
                        .invalidate_subtree_targets(&self.document, &subtrees);
                }
            }
            self.style_cache.borrow_mut().begin_frame();
            self.layout_cache.begin_frame();
            self.style_version = version;
            self.style_environment = Some(rules.environment);
            let mut geometry = layout::LayoutGeometry::default();
            let node_images = NodeImages {
                bitmaps: &self.node_bitmaps,
                fallback: images,
            };
            let list = layout::display_list_with_retained_layout_and_root(
                &self.document,
                width,
                height,
                text,
                self.rules.as_ref().expect("stylesheets initialized"),
                Some(&node_images),
                Some(&mut geometry),
                &self.scrolls,
                &self.style_cache,
                Some(&mut self.layout_cache),
                self.presentation_root,
                self.canvas_background,
            )?;
            self.frame_id += 1;
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
    use alloc::vec;
    use core::cell::Cell;

    struct NoText;

    #[test]
    fn client_rect_bounds_preserve_degenerate_fragments_and_all_empty_fallback() {
        let point = Rect {
            x: -50.0,
            y: -50.0,
            width: 0.0,
            height: 0.0,
        };
        let line = Rect {
            x: 40.0,
            y: 30.0,
            width: 0.0,
            height: 60.0,
        };
        let box_rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 20.0,
            height: 10.0,
        };
        assert_eq!(
            RenderSession::client_rect_bounds([point, line, box_rect].into_iter()),
            Some(Rect {
                x: 10.0,
                y: 20.0,
                width: 30.0,
                height: 70.0
            }),
        );
        assert_eq!(
            RenderSession::client_rect_bounds([line, point].into_iter()),
            Some(line),
        );
        assert_eq!(RenderSession::client_rect_bounds(core::iter::empty()), None);
    }

    #[test]
    fn canvas_dimensions_follow_html_integer_prefix_rules() {
        for (input, expected) in [
            (None, 300),
            (Some(""), 300),
            (Some("\t+12px"), 12),
            (Some("12.5"), 12),
            (Some("-1"), 300),
            (Some("-0"), 0),
            (Some("4294967296"), 300),
            (Some("\u{a0}12"), 300),
        ] {
            assert_eq!(layout::canvas_dimension(input, 300), expected);
        }
    }

    #[test]
    fn canvas_bitmaps_use_replaced_geometry_and_invalidate_retained_paint() {
        let document = crate::html::parse("<body style='margin:0'><canvas id='c' width='2' height='1' style='width:4px;height:2px'>fallback</canvas></body>", 32).unwrap();
        let node = crate::selector::query_selector(&document, document.root(), "#c")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let red = Arc::new(ImageData {
            width: 2,
            height: 1,
            pixels: alloc::vec![255, 0, 0, 255, 255, 0, 0, 255].into(),
        });
        session.set_node_bitmap(node, Some(red.clone())).unwrap();
        let revision = session.paint_revision();
        let list = session.display_list(10, 10, &NoText).unwrap();
        let (rect, image) = list
            .0
            .iter()
            .find_map(|command| match command {
                crate::paint::Command::Image { rect, image } => Some((*rect, image.clone())),
                _ => None,
            })
            .expect("canvas bitmap in retained display list");
        assert_eq!((rect.width, rect.height), (4.0, 2.0));
        assert!(Arc::ptr_eq(&red, &image));
        let blue = Arc::new(ImageData {
            width: 2,
            height: 1,
            pixels: alloc::vec![0, 0, 255, 255, 0, 0, 255, 255].into(),
        });
        session.set_node_bitmap(node, Some(blue.clone())).unwrap();
        assert_ne!(session.paint_revision(), revision);
        let list = session.display_list(10, 10, &NoText).unwrap();
        assert!(list.0.iter().any(|command| matches!(command, crate::paint::Command::Image { image, .. } if Arc::ptr_eq(image, &blue))));
    }

    #[test]
    fn loaded_image_bitmap_drives_natural_geometry_and_same_pixels_without_refetch() {
        let document = crate::html::parse("<body style='margin:0'><img id=i src=remote.png style='width:4px'><div id=d></div></body>", 32).unwrap();
        let image_node = crate::selector::query_selector(&document, document.root(), "#i")
            .unwrap()
            .unwrap();
        let div = crate::selector::query_selector(&document, document.root(), "#d")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let pixels = Arc::new(ImageData {
            width: 2,
            height: 1,
            pixels: alloc::vec![255, 0, 0, 255, 255, 0, 0, 255],
        });
        assert!(session.set_node_bitmap(div, Some(pixels.clone())).is_err());
        session
            .set_node_bitmap(image_node, Some(pixels.clone()))
            .unwrap();
        let frame = session.frame_id();
        let revision = session.paint_revision();
        session
            .set_node_bitmap(image_node, Some(pixels.clone()))
            .unwrap();
        assert_eq!(session.frame_id(), frame);
        assert_eq!(session.paint_revision(), revision);
        let list = session.display_list(10, 10, &NoText).unwrap();
        let (rect, published) = list
            .0
            .iter()
            .find_map(|command| match command {
                crate::paint::Command::Image { rect, image } => Some((*rect, image)),
                _ => None,
            })
            .expect("loaded image is painted without a URL resolver");
        assert_eq!((rect.width, rect.height), (4.0, 2.0));
        assert!(Arc::ptr_eq(&pixels, published));
        session.set_node_bitmap(image_node, None).unwrap();
        assert_ne!(session.frame_id(), frame);
        assert_eq!(
            session.display_list(10, 10, &NoText),
            Err(LayoutError::ImageFailed)
        );
    }

    #[test]
    fn shared_styles_retain_on_text_updates_and_invalidate_on_cascade_inputs() {
        let document = crate::html::parse(
            "<style>body{margin:0}p{height:10px}</style><p>one</p><p>two</p><p>three</p>",
            64,
        )
        .unwrap();
        let p = crate::selector::query_selector(&document, document.root(), "p")
            .unwrap()
            .unwrap();
        let text = document.first_child(p).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        let first = session.style_cache_stats();
        assert!(first.shared_hits >= 2);
        assert!(first.unique_styles < first.styled_nodes);
        session
            .document_mut()
            .replace_data(text, "changed")
            .unwrap();
        session.display_list(100, 100, &NoText).unwrap();
        let updated = session.style_cache_stats();
        assert_eq!(updated.computed_styles, 0);
        assert!(updated.cache_hits > 0);
        session
            .document_mut()
            .set_attribute(p, "style", "height:20px;background:red")
            .unwrap();
        session.display_list(100, 100, &NoText).unwrap();
        assert!(session.style_cache_stats().computed_styles > 0);
        assert_eq!(session.computed_style(p).unwrap().height, Some(20.0));
        session.display_list(200, 100, &NoText).unwrap();
        assert!(session.style_cache_stats().computed_styles > 0);
    }

    #[test]
    fn retained_layout_preserves_shared_float_context_after_sibling_mutation() {
        let fixture = "<style>body{margin:0}main{width:60px}.f{float:left;width:30px;height:20px;background:red}</style><main><div><i class=f></i><i class=f></i></div><div><i class=f></i></div><div id=tail style='clear:both;height:10px;background:green'></div></main>";
        let document = crate::html::parse(fixture, 64).unwrap();
        let tail = crate::selector::get_element_by_id(&document, document.root(), "tail")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        session
            .document_mut()
            .set_attribute(tail, "style", "clear:both;height:12px;background:green")
            .unwrap();
        let updated = session.display_list(100, 100, &NoText).unwrap().clone();
        let reference = crate::html::parse(
            &fixture.replace(
                "height:10px;background:green",
                "height:12px;background:green",
            ),
            64,
        )
        .unwrap();
        let reference = RenderSession::new(reference)
            .display_list(100, 100, &NoText)
            .unwrap()
            .clone();
        assert_eq!(
            updated, reference,
            "cached sibling paints must not discard shared float exclusions"
        );
    }

    #[test]
    fn retained_layout_reuses_unchanged_siblings_after_text_mutation() {
        let document = crate::html::parse(
            "<style>body{margin:0}p{height:10px;background:#eee}</style><p>one</p><p>two</p><p>three</p>",
            64,
        )
        .unwrap();
        let first = crate::selector::query_selector(&document, document.root(), "p")
            .unwrap()
            .unwrap();
        let text = document.first_child(first).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        let initial = session.layout_cache_stats();
        assert!(
            initial.entries >= 3,
            "first layout should retain paragraph fragments: {initial:?}"
        );
        assert!(initial.bytes > 0 && initial.bytes <= 1024 * 1024);

        session
            .document_mut()
            .replace_data(text, "changed")
            .unwrap();
        session.display_list(100, 100, &NoText).unwrap();
        let updated = session.layout_cache_stats();
        assert!(
            updated.hits >= 2,
            "unchanged p siblings should replay: {updated:?}"
        );
        assert!(updated.entries >= 3);
        assert!(updated.bytes <= 1024 * 1024);
    }

    #[test]
    fn attribute_mutation_retains_siblings_and_invalidates_descendant_selectors() {
        let fixture = "<style>body{margin:0}p{height:10px;margin:0}.child{display:block;height:10px;background:blue}.on .child{background:red}</style><p><span class='child'>one</span></p><p><span class='child'>two</span></p><p><span class='child'>three</span></p>";
        let document = crate::html::parse(fixture, 64).unwrap();
        let first = crate::selector::query_selector(&document, document.root(), "p")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        session
            .document_mut()
            .set_attribute(first, "class", "on")
            .unwrap();
        let updated = session.display_list(100, 100, &NoText).unwrap().clone();
        let stats = session.layout_cache_stats();
        assert!(
            stats.hits >= 2,
            "unchanged siblings should replay: {stats:?}"
        );
        let reference =
            crate::html::parse(&fixture.replacen("<p>", "<p class='on'>", 1), 64).unwrap();
        let reference = RenderSession::new(reference)
            .display_list(100, 100, &NoText)
            .unwrap()
            .clone();
        assert_eq!(
            updated, reference,
            "attribute ancestry changes must repaint descendants"
        );
    }

    #[test]
    fn slot_distribution_mutations_invalidate_the_composed_host_subtree() {
        fn fixture(
            child_slot: &str,
            first_slot_name: &str,
            second_slot_name: &str,
        ) -> (Document, NodeId, NodeId) {
            let html = alloc::format!(
                "<body style='margin:0'><p style='display:block;width:12px;height:6px;background:purple'></p><div id='host' style='display:block;width:40px'><i id='light' slot='{child_slot}' style='display:block;width:16px;height:10px;background:red'>ink</i></div></body>"
            );
            let mut document = crate::html::parse(&html, 64).unwrap();
            let host = crate::selector::query_selector(&document, document.root(), "#host")
                .unwrap()
                .unwrap();
            let child = crate::selector::query_selector(&document, document.root(), "#light")
                .unwrap()
                .unwrap();
            let shadow_root = document
                .attach_shadow(host, crate::shadow::ShadowMode::Open)
                .unwrap();
            let shadow = crate::html::parse_fragment(
                &mut document,
                &alloc::format!(
                    "<slot name='{first_slot_name}' style='display:block;width:40px;height:10px;background:blue'></slot><slot name='{second_slot_name}' style='display:block;width:40px;height:10px;background:green'></slot>"
                ),
            )
            .unwrap();
            let first_slot = document.first_child(shadow).unwrap().unwrap();
            document.append(shadow_root, shadow).unwrap();
            (document, child, first_slot)
        }

        // Changing a light child's slot moves it from one rendered slot to a
        // sibling. The former slot is outside the child's new composed ancestry.
        let (document, child, _) = fixture("a", "a", "b");
        let mut session = RenderSession::new(document);
        session.display_list(80, 80, &NoText).unwrap();
        session
            .document_mut()
            .set_attribute(child, "slot", "b")
            .unwrap();
        let updated = session.display_list(80, 80, &NoText).unwrap().clone();
        assert!(
            session.layout_cache_stats().hits > 0,
            "unrelated siblings should remain reusable"
        );
        let (reference, _, _) = fixture("b", "a", "b");
        let reference = RenderSession::new(reference)
            .display_list(80, 80, &NoText)
            .unwrap()
            .clone();
        assert_eq!(
            updated, reference,
            "slot attribute change must repaint both slots"
        );

        // Renaming the first of duplicate-name slots transfers its assigned
        // children to the next matching slot, which is not a descendant of the
        // mutated slot either.
        let (document, _, first_slot) = fixture("c", "c", "c");
        let mut session = RenderSession::new(document);
        session.display_list(80, 80, &NoText).unwrap();
        session
            .document_mut()
            .set_attribute(first_slot, "name", "x")
            .unwrap();
        let updated = session.display_list(80, 80, &NoText).unwrap().clone();
        assert!(
            session.layout_cache_stats().hits > 0,
            "unrelated siblings should remain reusable"
        );
        let (reference, _, _) = fixture("c", "x", "c");
        let reference = RenderSession::new(reference)
            .display_list(80, 80, &NoText)
            .unwrap()
            .clone();
        assert_eq!(
            updated, reference,
            "slot name change must repaint assignment destination"
        );
    }

    #[test]
    fn appended_rows_reuse_existing_layout_fragments() {
        let fixture = "<style>body{margin:0}p{height:10px;margin:0;background:blue}</style><p>one</p><p>two</p>";
        let document = crate::html::parse(fixture, 64).unwrap();
        let body = crate::selector::query_selector(&document, document.root(), "body")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        let document = session.document_mut();
        let row = document
            .create(NodeKind::Element {
                namespace: crate::Namespace::Html,
                name: "p".into(),
                attributes: Vec::new(),
            })
            .unwrap();
        let text = document.create(NodeKind::Text("three".into())).unwrap();
        document.append(row, text).unwrap();
        document.append(body, row).unwrap();
        let updated = session.display_list(100, 100, &NoText).unwrap().clone();
        assert!(session.layout_cache_stats().hits >= 2);
        let reference = crate::html::parse(&alloc::format!("{fixture}<p>three</p>"), 64).unwrap();
        let reference = RenderSession::new(reference)
            .display_list(100, 100, &NoText)
            .unwrap()
            .clone();
        assert_eq!(updated, reference);
    }

    #[test]
    fn reparented_fragments_refresh_descendant_ancestry_styles() {
        let styles = "<style>body{margin:0}.left,.right{height:20px;width:40px}.child,.grand{display:block;height:10px}.left .grand{background:red}.right .grand{background:blue}p{height:10px;margin:0;background:green}</style>";
        let fixture = alloc::format!(
            "{styles}<div class='left'><div class='child'><span class='grand'>ink</span></div></div><div class='right'></div><p>static</p>"
        );
        let document = crate::html::parse(&fixture, 64).unwrap();
        let child = crate::selector::query_selector(&document, document.root(), ".child")
            .unwrap()
            .unwrap();
        let right = crate::selector::query_selector(&document, document.root(), ".right")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        session.document_mut().append(right, child).unwrap();
        let updated = session.display_list(100, 100, &NoText).unwrap().clone();
        assert!(session.layout_cache_stats().hits >= 1);
        let reference = alloc::format!(
            "{styles}<div class='left'></div><div class='right'><div class='child'><span class='grand'>ink</span></div></div><p>static</p>"
        );
        let reference = crate::html::parse(&reference, 64).unwrap();
        let reference = RenderSession::new(reference)
            .display_list(100, 100, &NoText)
            .unwrap()
            .clone();
        assert_eq!(updated, reference);
    }

    #[test]
    fn sibling_attribute_selectors_disable_local_fragment_reuse() {
        let fixture = "<style>body{margin:0}p{height:10px;margin:0;background:blue}.on ~ p{background:red}</style><p>one</p><p>two</p><p>three</p>";
        let document = crate::html::parse(fixture, 64).unwrap();
        let first = crate::selector::query_selector(&document, document.root(), "p")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        session
            .document_mut()
            .set_attribute(first, "class", "on")
            .unwrap();
        let updated = session.display_list(100, 100, &NoText).unwrap().clone();
        assert_eq!(session.layout_cache_stats().hits, 0);
        let reference =
            crate::html::parse(&fixture.replacen("<p>", "<p class='on'>", 1), 64).unwrap();
        let reference = RenderSession::new(reference)
            .display_list(100, 100, &NoText)
            .unwrap()
            .clone();
        assert_eq!(updated, reference);
    }

    #[test]
    fn text_sensitive_selectors_do_not_reuse_stale_styles() {
        let document = crate::html::parse(
            "<style>p{height:10px}p:empty{height:20px}</style><p>x</p>",
            64,
        )
        .unwrap();
        let p = crate::selector::query_selector(&document, document.root(), "p")
            .unwrap()
            .unwrap();
        let text = document.first_child(p).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        session.document_mut().replace_data(text, "").unwrap();
        session.display_list(100, 100, &NoText).unwrap();
        assert!(session.style_cache_stats().computed_styles > 0);
        assert_eq!(session.computed_style(p).unwrap().height, Some(20.0));
    }

    #[test]
    fn adopted_stylesheets_join_the_live_cascade_and_validate_atomically() {
        let document = crate::html::parse("<div></div>", 16).unwrap();
        let div = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        assert_eq!(session.computed_style(div).unwrap().width, None);
        session
            .set_adopted_stylesheets(None, vec!["div { width: 12px }".into()])
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        session
            .set_adopted_stylesheets(None, vec!["div { width: 4px".into()])
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(4.0));
        let excessive_nesting = alloc::format!(
            "{}div{{width:2px}}{}",
            "@media screen{".repeat(33),
            "}".repeat(33)
        );
        assert!(session
            .set_adopted_stylesheets(None, vec![excessive_nesting])
            .is_err());
        assert_eq!(session.computed_style(div).unwrap().width, Some(4.0));
        session.set_adopted_stylesheets(None, Vec::new()).unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, None);
    }

    #[test]
    fn stylesheet_types_and_adopted_font_definitions_share_live_collection() {
        let document = crate::html::parse(
            "<!doctype html><style type=text/plain>div{width:80px}</style><style disabled>div{height:9px}</style><style>@layer base;@layer base{@font-face{font-family:Example;src:url(base.woff)}}div{width:4px}</style><div></div>",
            32,
        ).unwrap();
        let div = crate::selector::query_selector(&document, document.root(), "div")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.set_document_base_url(Some(Arc::from("https://example.test/css/page.html")));
        assert_eq!(session.computed_style(div).unwrap().height, Some(9.0));
        assert_eq!(session.computed_style(div).unwrap().width, Some(4.0));
        session.set_adopted_stylesheets(None, vec![
            "@layer override{@font-face{font-family:Example;src:url(override.woff)}}div{width:12px}".into(),
        ]).unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        let faces = session.font_faces().unwrap();
        assert_eq!(faces.len(), 2);
        assert!(faces[0].layer_path < faces[1].layer_path);
        assert!(faces
            .iter()
            .all(|face| face.source_url.as_deref() == Some("https://example.test/css/page.html")));
        session.set_adopted_stylesheets(None, Vec::new()).unwrap();
        assert_eq!(session.font_faces().unwrap().len(), 1);
        assert_eq!(session.computed_style(div).unwrap().width, Some(4.0));
    }

    #[test]
    fn css_font_face_descriptor_override_preserves_rule_identity_and_paint_order() {
        let document = crate::html::parse(
            "<style>@layer base;@layer base{@font-face{font-family:Example;src:url(base.woff);font-weight:400}}@font-face{font-family:Other;src:url(other.woff)}</style>",
            32,
        )
        .unwrap();
        let mut session = RenderSession::new(document);
        session.set_document_base_url(Some(Arc::from("https://example.test/css/page.html")));
        let before = session.font_faces().unwrap();
        let original = before
            .iter()
            .find(|face| face.family.as_ref() == "Example")
            .unwrap()
            .clone();
        let identity = original.identity.clone().unwrap();
        let mut descriptors = original.descriptors.clone();
        descriptors.set_family_argument("Renamed Example").unwrap();
        descriptors.set("weight", "700").unwrap();
        let frame = session.frame_id();
        let paint_revision = session.paint_revision();

        assert!(session
            .set_font_face_descriptors(&identity, descriptors.clone())
            .unwrap());
        assert!(session.frame_id() > frame);
        assert!(session.paint_revision() > paint_revision);

        let after = session.font_faces().unwrap();
        let updated = after
            .iter()
            .find(|face| face.identity.as_ref() == Some(&identity))
            .unwrap();
        assert_eq!(updated.family.as_ref(), "Renamed Example");
        assert_eq!(updated.weight_range, [700, 700]);
        assert_eq!(updated.sources, original.sources);
        assert_eq!(updated.source_url, original.source_url);
        assert_eq!(updated.layer_path, original.layer_path);
        assert_eq!(updated.layers, original.layers);
        assert_eq!(updated.source_order, original.source_order);
        assert_eq!(updated.media, original.media);
        assert_eq!(updated.supports, original.supports);
        assert_eq!(after.len(), before.len());
    }

    #[test]
    fn css_font_face_override_is_removed_when_its_source_revision_changes() {
        let document = crate::html::parse("<main></main>", 16).unwrap();
        let mut session = RenderSession::new(document);
        session
            .set_adopted_stylesheets(
                None,
                vec!["@font-face{font-family:Original;src:url(old.woff)}".into()],
            )
            .unwrap();
        let original = session.font_faces().unwrap().remove(0);
        let identity = original.identity.clone().unwrap();
        let mut descriptors = original.descriptors.clone();
        descriptors.set_family_argument("Temporary").unwrap();
        assert!(session
            .set_font_face_descriptors(&identity, descriptors.clone())
            .unwrap());
        assert_eq!(
            session.font_faces().unwrap()[0].family.as_ref(),
            "Temporary"
        );

        session
            .set_adopted_stylesheets(
                None,
                vec!["@font-face{font-family:Replacement;src:url(new.woff)}".into()],
            )
            .unwrap();
        let replacement = session.font_faces().unwrap().remove(0);
        assert_ne!(replacement.identity.as_ref(), Some(&identity));
        assert_eq!(replacement.family.as_ref(), "Replacement");
        assert!(!session
            .font_face_descriptor_overrides
            .iter()
            .any(|(current, _)| current == &identity));
        assert!(!session
            .set_font_face_descriptors(&identity, descriptors)
            .unwrap());
        assert!(!session
            .set_font_face_descriptors(&css::FontFaceIdentity::Manual(99), replacement.descriptors,)
            .unwrap());
    }

    #[test]
    fn linked_stylesheets_follow_owner_order_href_and_disabled_state() {
        let document = crate::html::parse("<!doctype html><head><style>div{width:4px}</style><link rel=stylesheet href=first.css><style>div{width:8px}</style><link id=last rel=stylesheet href=last.css></head><body><div></div></body>", 32).unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let first = find("link");
        let last = find("#last");
        let div = find("div");
        let mut session = RenderSession::new(document);
        session
            .set_linked_stylesheet(first, Some("div{width:6px}".into()))
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(8.0));
        session
            .set_linked_stylesheet(last, Some("div{width:12px}".into()))
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        session
            .document_mut()
            .set_attribute(last, "disabled", "")
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(8.0));
        session
            .document_mut()
            .remove_attribute(last, "disabled")
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        session
            .document_mut()
            .set_attribute(last, "media", "print")
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(8.0));
        session
            .document_mut()
            .remove_attribute(last, "media")
            .unwrap();
        session
            .document_mut()
            .set_attribute(last, "href", "new.css")
            .unwrap();
        assert_eq!(session.linked_stylesheet(last), None);
        assert_eq!(session.computed_style(div).unwrap().width, Some(8.0));
        session
            .set_linked_stylesheet(last, Some("div{width:16px}".into()))
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(16.0));
        session
            .set_linked_stylesheet(last, Some("div{width:1px".into()))
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(1.0));
        let excessive_nesting = alloc::format!(
            "{}div{{width:2px}}{}",
            "@media screen{".repeat(33),
            "}".repeat(33)
        );
        assert!(session
            .set_linked_stylesheet(last, Some(excessive_nesting))
            .is_err());
        assert_eq!(session.computed_style(div).unwrap().width, Some(1.0));
        session.document_mut().remove(last).unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(8.0));
    }

    #[test]
    fn stylesheet_graphs_keep_owner_order_original_text_and_stale_snapshot_rules() {
        let document = crate::html::parse("<style>div{width:4px}</style><link rel=stylesheet href=first.css><style id=inline>@import 'child.css';div{height:7px}</style><link id=last rel=stylesheet href=last.css><div></div>", 32).unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let first = find("link");
        let inline = find("#inline");
        let last = find("#last");
        let div = find("div");
        let inline_text = document.first_child(inline).unwrap().unwrap();
        let leaf = |url: &str, text: &str| css::StylesheetSource {
            url: Arc::from(url),
            text: Arc::from(text),
            imports: Vec::new(),
        };
        let mut session = RenderSession::new(document);
        session
            .set_stylesheet_source(
                first,
                Some(leaf("https://example.test/first.css", "div{width:6px}")),
            )
            .unwrap();
        let text = "@import 'child.css';div{height:7px}";
        let rule = css::imports(text).unwrap().remove(0);
        let graph = css::StylesheetSource {
            url: Arc::from("https://example.test/page.html"),
            text: Arc::from(text),
            imports: alloc::vec![css::LoadedImport {
                rule,
                source: Some(alloc::boxed::Box::new(leaf(
                    "https://example.test/child.css",
                    "div{width:12px}"
                ))),
            }],
        };
        let version = session.document().version();
        session
            .set_stylesheet_source(inline, Some(graph.clone()))
            .unwrap();
        assert_eq!(session.document().version(), version);
        assert_eq!(
            session.document().kind(inline_text).unwrap(),
            &NodeKind::Text(text.into())
        );
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        assert_eq!(session.computed_style(div).unwrap().height, Some(7.0));
        let frame = session.frame_id();
        session.set_stylesheet_source(inline, Some(graph)).unwrap();
        assert_eq!(session.frame_id(), frame);
        session
            .set_stylesheet_source(
                last,
                Some(leaf("https://example.test/last.css", "div{width:14px}")),
            )
            .unwrap();
        assert_eq!(session.computed_style(div).unwrap().width, Some(14.0));
        session
            .document_mut()
            .set_attribute(last, "href", "changed.css")
            .unwrap();
        assert!(session.stylesheet_source(last).is_none());
        assert_eq!(session.computed_style(div).unwrap().width, Some(12.0));
        session
            .document_mut()
            .replace_data(inline_text, "div{width:18px}")
            .unwrap();
        assert!(session.stylesheet_source(inline).is_none());
        assert_eq!(session.computed_style(div).unwrap().width, Some(18.0));
        let bad = leaf("https://example.test/page.html", "div{width:99px}");
        assert!(session.set_stylesheet_source(inline, Some(bad)).is_err());
        assert_eq!(session.computed_style(div).unwrap().width, Some(18.0));
    }

    #[test]
    fn transformed_and_relative_flow_paint_and_hits_follow_stacking_order() {
        for context in [
            "transform:translate(0,10px)",
            "position:relative;top:10px",
            "opacity:0.5",
        ] {
            let document = crate::html::parse(&alloc::format!(
                "<style>body{{margin:0}}#a{{width:30px;height:30px;background:red;{context}}}#b{{width:30px;height:30px;background:blue;margin-top:-20px}}</style><div id=a></div><div id=b></div>"
            ), 64).unwrap();
            let a = crate::selector::query_selector(&document, document.root(), "#a")
                .unwrap()
                .unwrap();
            let mut session = RenderSession::new(document);
            let list = session.display_list(100, 100, &NoText).unwrap();
            let red = list.0.iter().position(|command| matches!(command, crate::paint::Command::FillRect { color, .. } if color.r == 255 && color.b == 0)).unwrap();
            let blue = list.0.iter().position(|command| matches!(command, crate::paint::Command::FillRect { color, .. } if color.b == 255 && color.r == 0)).unwrap();
            assert!(red > blue, "{context}");
            assert_eq!(session.hit_test(5.0, 20.0), Some(a), "{context}");
        }
    }

    #[test]
    fn positioned_positive_z_levels_sort_numerically_and_keep_hit_order() {
        let document = crate::html::parse(
            "<style>body{margin:0}div{position:absolute;left:0;top:0;width:30px;height:30px}#a{background:red;z-index:10}#b{background:blue;z-index:2}</style><div id=a></div><div id=b></div>", 64
        ).unwrap();
        let a = crate::selector::query_selector(&document, document.root(), "#a")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        assert_eq!(session.hit_test(5.0, 5.0), Some(a));
    }

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
        let document = crate::html::parse("<body style='margin:0'><div style='display:flex;gap:5px'><div style='width:5px;height:10px'></div><div id='outer' style='width:10px;height:10px;overflow:hidden;border-radius:5px'><div id='inner' style='width:10px;height:10px'></div></div></div>", 32).unwrap();
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
    fn live_form_validity_generations_refresh_retained_styles_without_dom_mutation() {
        use alloc::rc::Rc;

        struct LiveState {
            invalid: Cell<bool>,
            generation: Cell<u64>,
        }

        let mut document = crate::html::parse(
            "<style>body{margin:0}input{display:block;width:3px;height:2px;padding:0;border:0;background:red}input:invalid{background:blue}</style><body><input pattern='a' value='a'><input pattern='a' value='a'></body>",
            24,
        ).unwrap();
        let nodes =
            crate::selector::query_selector_all(&document, document.root(), "input").unwrap();
        let second = nodes[1];
        let state = Rc::new(LiveState {
            invalid: Cell::new(false),
            generation: Cell::new(1),
        });
        let weak_read = Rc::downgrade(&state);
        let weak_generation = Rc::downgrade(&state);
        document.set_validity_resolver(
            Rc::new(move |_, node| {
                weak_read
                    .upgrade()
                    .map(|state| crate::forms::ValidityState {
                        pattern_mismatch: node == second && state.invalid.get(),
                        ..Default::default()
                    })
            }),
            Rc::new(move || {
                weak_generation
                    .upgrade()
                    .map_or(0, |state| state.generation.get())
            }),
        );
        let mut session = RenderSession::new(document);
        session.display_list(20, 20, &NoText).unwrap();
        let frame = session.frame_id();
        let revision = session.paint_revision();
        let document_version = session.document().version();
        session.display_list(20, 20, &NoText).unwrap();
        assert_eq!(session.frame_id(), frame);
        assert_eq!(session.paint_revision(), revision);

        state.invalid.set(true);
        state.generation.set(2);
        let list = session.display_list(20, 20, &NoText).unwrap();
        let has_fill =
            |red, blue| {
                list.0.iter().any(|command| matches!(command,
            crate::paint::Command::FillRect { rect, color }
            if rect.width == 3.0 && rect.height == 2.0 && color.r == red && color.b == blue
        ))
            };
        assert!(has_fill(255, 0), "valid sibling keeps its own style");
        assert!(has_fill(0, 255), "dirty invalid sibling gets a fresh style");
        assert_eq!(session.document().version(), document_version);
        assert_ne!(session.paint_revision(), revision);
        assert_eq!(session.computed_style(second).unwrap().background.b, 255);

        drop(state);
        session.display_list(20, 20, &NoText).unwrap();
        assert_eq!(session.computed_style(second).unwrap().background.r, 255);
    }

    #[test]
    fn live_interaction_generation_refreshes_only_observable_styles() {
        let document = crate::html::parse(
            "<style>body{margin:0}button{display:block;width:4px;height:3px;padding:0;border:0;background:red}button:focus-visible{background:blue}</style><body><button id='target'></button></body>",
            24,
        )
        .unwrap();
        let target = crate::selector::get_element_by_id(&document, document.root(), "target")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(12, 8, &NoText).unwrap();
        let frame = session.frame_id();
        let paint_revision = session.paint_revision();
        let document_version = session.document().version();

        session
            .document_mut()
            .set_interaction_state(crate::interaction::InteractionState {
                focused: Some(target),
                focus_visible: Some(target),
                ..Default::default()
            });
        let style = session.computed_style(target).unwrap();

        assert_eq!(style.background.b, 255);
        assert!(session.frame_id() > frame);
        assert!(session.paint_revision() > paint_revision);
        assert_eq!(session.document().version(), document_version);
    }

    #[test]
    fn live_checkedness_generations_refresh_selector_styles_without_dom_mutation() {
        use alloc::rc::Rc;

        struct LiveState {
            checked: Cell<bool>,
            generation: Cell<u64>,
        }

        let mut document = crate::html::parse(
            "<style>body{margin:0}input{display:block;width:3px;height:2px;padding:0;border:0;background:red}input:checked{background:blue}</style><body><input type='checkbox'><input id='target' type='checkbox'></body>",
            24,
        )
        .unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let state = Rc::new(LiveState {
            checked: Cell::new(false),
            generation: Cell::new(1),
        });
        let weak_validity = Rc::downgrade(&state);
        let weak_generation = Rc::downgrade(&state);
        document.set_validity_resolver(
            Rc::new(|_, _| None),
            Rc::new(move || {
                weak_generation
                    .upgrade()
                    .map_or(0, |state| state.generation.get())
            }),
        );
        document.set_form_selector_state_resolver(Rc::new(move |_, node| {
            let state = weak_validity.upgrade()?;
            Some(crate::forms::FormSelectorState {
                checkedness: (node == target).then(|| state.checked.get()),
                selectedness: None,
                single_select_option: None,
                user_validity_interacted: None,
                placeholder_shown: None,
                auto_value_directionality: None,
            })
        }));

        let mut session = RenderSession::new(document);
        let before_version = session.document().version();
        let has_fill =
            |list: &DisplayList, red: u8, blue: u8| {
                list.0.iter().any(|command| matches!(command,
                crate::paint::Command::FillRect { rect, color }
                if rect.width == 3.0 && rect.height == 2.0 && color.r == red && color.b == blue
            ))
            };
        let (before_has_red, before_has_blue) = {
            let before = session.display_list(20, 20, &NoText).unwrap();
            (has_fill(before, 255, 0), has_fill(before, 0, 255))
        };
        let revision = session.paint_revision();
        assert!(before_has_red);
        assert!(!before_has_blue);

        state.checked.set(true);
        state.generation.set(2);
        let after = session.display_list(20, 20, &NoText).unwrap();
        assert!(has_fill(&after, 255, 0), "the unchecked sibling stays red");
        assert!(
            has_fill(&after, 0, 255),
            "the live checked control turns blue"
        );
        assert_eq!(session.document().version(), before_version);
        assert_ne!(session.paint_revision(), revision);
        assert_eq!(session.computed_style(target).unwrap().background.b, 255);
    }

    #[test]
    fn live_auto_directionality_generations_refresh_selector_styles() {
        use alloc::rc::Rc;
        use core::cell::Cell;

        struct LiveDirection {
            rtl: Cell<bool>,
            generation: Cell<u64>,
        }

        let mut document = crate::html::parse(
            "<style>body{margin:0}input{display:block;width:3px;height:2px;padding:0;border:0;background:red}input:dir(rtl){background:blue}</style><body><input id='target' dir='auto' value='English'></body>",
            64,
        )
        .unwrap();
        let target = crate::selector::get_element_by_id(&document, document.root(), "target")
            .unwrap()
            .unwrap();
        let state = Rc::new(LiveDirection {
            rtl: Cell::new(false),
            generation: Cell::new(1),
        });
        let weak_value = Rc::downgrade(&state);
        let weak_generation = Rc::downgrade(&state);
        document.set_validity_resolver(
            Rc::new(|_, _| None),
            Rc::new(move || {
                weak_generation
                    .upgrade()
                    .map_or(0, |state| state.generation.get())
            }),
        );
        document.set_form_selector_state_resolver(Rc::new(move |_, node| {
            let state = weak_value.upgrade()?;
            Some(crate::forms::FormSelectorState {
                checkedness: None,
                selectedness: None,
                single_select_option: None,
                user_validity_interacted: None,
                placeholder_shown: None,
                auto_value_directionality: (node == target).then(|| {
                    if state.rtl.get() {
                        crate::directionality::Direction::Rtl
                    } else {
                        crate::directionality::Direction::Ltr
                    }
                }),
            })
        }));

        let mut session = RenderSession::new(document);
        let before_version = session.document().version();
        let has_fill =
            |list: &DisplayList, red, blue| {
                list.0.iter().any(|command| matches!(command,
                crate::paint::Command::FillRect { rect, color }
                if rect.width == 3.0 && rect.height == 2.0 && color.r == red && color.b == blue
            ))
            };
        {
            let before = session.display_list(20, 20, &NoText).unwrap();
            assert!(has_fill(before, 255, 0));
        }
        let revision = session.paint_revision();

        state.rtl.set(true);
        state.generation.set(2);
        let after = session.display_list(20, 20, &NoText).unwrap();
        assert!(has_fill(&after, 0, 255));
        assert_eq!(session.document().version(), before_version);
        assert_ne!(session.paint_revision(), revision);
        assert_eq!(session.computed_style(target).unwrap().background.b, 255);
    }

    #[test]
    fn live_auto_directionality_ua_styles_refresh_without_selector_rules() {
        use alloc::rc::Rc;
        use core::cell::Cell;

        struct LiveDirection {
            rtl: Cell<bool>,
            generation: Cell<u64>,
        }

        let mut document = crate::html::parse(
            "<body><input id='target' dir='auto' type='text' value='English'></body>",
            32,
        )
        .unwrap();
        let target = crate::selector::get_element_by_id(&document, document.root(), "target")
            .unwrap()
            .unwrap();
        let state = Rc::new(LiveDirection {
            rtl: Cell::new(false),
            generation: Cell::new(1),
        });
        let weak_value = Rc::downgrade(&state);
        let weak_generation = Rc::downgrade(&state);
        document.set_validity_resolver(
            Rc::new(|_, _| None),
            Rc::new(move || {
                weak_generation
                    .upgrade()
                    .map_or(0, |state| state.generation.get())
            }),
        );
        document.set_form_selector_state_resolver(Rc::new(move |_, node| {
            let state = weak_value.upgrade()?;
            Some(crate::forms::FormSelectorState {
                checkedness: None,
                selectedness: None,
                single_select_option: None,
                user_validity_interacted: None,
                placeholder_shown: None,
                auto_value_directionality: (node == target).then(|| {
                    if state.rtl.get() {
                        crate::directionality::Direction::Rtl
                    } else {
                        crate::directionality::Direction::Ltr
                    }
                }),
            })
        }));

        let mut session = RenderSession::new(document);
        let before_version = session.document().version();
        let before = session.computed_style(target).unwrap();
        assert_eq!(before.direction(), css::Direction::Ltr);
        let before_revision = session.paint_revision();

        state.rtl.set(true);
        state.generation.set(2);
        let after = session.computed_style(target).unwrap();
        assert_eq!(after.direction(), css::Direction::Rtl);
        assert_ne!(session.paint_revision(), before_revision);
        assert_eq!(session.document().version(), before_version);
    }

    #[test]
    fn unused_form_validity_generations_preserve_retained_caches() {
        use alloc::rc::Rc;

        let mut document = crate::html::parse(
            "<style>div{width:3px;height:2px;background:red}</style><div></div>",
            16,
        )
        .unwrap();
        let generation = Rc::new(Cell::new(1));
        let source = generation.clone();
        document.set_validity_resolver(Rc::new(|_, _| None), Rc::new(move || source.get()));
        let mut session = RenderSession::new(document);
        session.display_list(20, 20, &NoText).unwrap();
        let frame = session.frame_id();
        let revision = session.paint_revision();
        let stats = session.style_cache_stats();
        generation.set(2);
        session.display_list(20, 20, &NoText).unwrap();
        assert_eq!(session.frame_id(), frame);
        assert_eq!(session.paint_revision(), revision);
        assert_eq!(session.style_cache_stats(), stats);
    }

    #[test]
    fn hit_test_pointer_events_inherit_and_override_without_removing_geometry() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='outer' style='pointer-events:none;width:20px;height:20px'><div id='inner' style='width:5px;height:5px'></div><div id='override' style='pointer-events:auto;width:5px;height:5px'></div></div>",
            24,
        ).unwrap();
        let find = |id| {
            crate::selector::get_element_by_id(&document, document.root(), id)
                .unwrap()
                .unwrap()
        };
        let outer = find("outer");
        let inner = find("inner");
        let explicit = find("override");
        let mut session = RenderSession::new(document);
        session.display_list(30, 30, &NoText).unwrap();
        assert!(!session.computed_style(outer).unwrap().pointer_events_auto);
        assert!(!session.computed_style(inner).unwrap().pointer_events_auto);
        assert!(
            session
                .computed_style(explicit)
                .unwrap()
                .pointer_events_auto
        );
        assert!(session.client_rects(inner).first().is_some());
        let mut hits = Vec::new();
        session.for_each_hit_test(2.0, 2.0, |node| {
            hits.push(node);
            true
        });
        assert!(!hits.contains(&inner));
        assert!(!hits.contains(&outer));
        assert_eq!(session.hit_test(2.0, 7.0), Some(explicit));
        session
            .document_mut()
            .set_attribute(outer, "style", "pointer-events:auto;width:20px;height:20px")
            .unwrap();
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(2.0, 2.0), Some(inner));
        session
            .document_mut()
            .set_attribute(inner, "style", "visibility:hidden;width:5px;height:5px")
            .unwrap();
        session.display_list(30, 30, &NoText).unwrap();
        assert_ne!(session.hit_test(2.0, 2.0), Some(inner));
        assert!(!session.client_rects(inner).is_empty());
    }

    #[test]
    fn hit_test_stack_visits_front_to_back_and_stops_at_the_requested_hit() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='outer' style='width:20px;height:20px;padding:2px'><div id='inner' style='width:5px;height:5px'></div></div>",
            16,
        )
        .unwrap();
        let outer = crate::selector::get_element_by_id(&document, document.root(), "outer")
            .unwrap()
            .unwrap();
        let inner = crate::selector::get_element_by_id(&document, document.root(), "inner")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        assert!(!session.for_each_hit_test(3.0, 3.0, |_| panic!("no completed frame")));
        session.display_list(30, 30, &NoText).unwrap();
        let mut hits = Vec::new();
        assert!(session.for_each_hit_test(3.0, 3.0, |node| {
            hits.push(node);
            true
        }));
        assert_eq!(hits.first(), Some(&inner));
        assert!(
            hits.iter().position(|node| *node == inner).unwrap()
                < hits.iter().position(|node| *node == outer).unwrap()
        );
        let mut visited = 0;
        assert!(session.for_each_hit_test(3.0, 3.0, |node| {
            visited += 1;
            assert_eq!(node, inner);
            false
        }));
        assert_eq!(visited, 1);
        assert!(!session.for_each_hit_test(f32::NAN, 3.0, |_| panic!("invalid point")));
        assert!(!session.for_each_hit_test(30.0, 3.0, |_| panic!("outside viewport")));
        session
            .document_mut()
            .set_attribute(inner, "hidden", "")
            .unwrap();
        assert!(!session.for_each_hit_test(3.0, 3.0, |_| panic!("stale frame")));
    }

    #[test]
    fn hit_test_uses_deepest_box_viewport_and_completed_version() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='outer' style='width:20px;height:20px;padding:2px'><div id='inner' style='width:5px;height:5px'></div><div id='hidden' style='display:none;width:20px;height:20px'></div></div>", 16).unwrap();
        let outer = crate::selector::query_selector(&document, document.root(), "#outer")
            .unwrap()
            .unwrap();
        let inner = crate::selector::query_selector(&document, document.root(), "#inner")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        assert_eq!(session.hit_test(3.0, 3.0), None);
        assert_eq!(session.hit_bounds(inner), None);
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(3.0, 3.0), Some(inner));
        let inner_bounds = session.hit_bounds(inner).unwrap();
        assert_eq!(inner_bounds.x, 2.0);
        assert_eq!(inner_bounds.y, 2.0);
        assert_eq!(inner_bounds.width, 5.0);
        assert_eq!(inner_bounds.height, 5.0);
        assert_eq!(session.untransformed_hit_bounds(inner), Some(inner_bounds));
        let hidden = crate::selector::query_selector(
            session.document(),
            session.document().root(),
            "#hidden",
        )
        .unwrap()
        .unwrap();
        assert_eq!(session.hit_bounds(hidden), None);
        assert_eq!(session.untransformed_hit_bounds(hidden), None);
        assert_eq!(session.hit_test(1.0, 1.0), Some(outer));
        assert_eq!(session.hit_test(7.0, 3.0), Some(outer));
        assert_eq!(session.hit_test(30.0, 3.0), None);
        assert_eq!(session.hit_test(f32::NAN, 3.0), None);
        session
            .document_mut()
            .set_attribute(inner, "style", "display:none")
            .unwrap();
        assert_eq!(session.hit_bounds(inner), None);
        assert_eq!(session.hit_test(3.0, 3.0), None);
        session.display_list(30, 30, &NoText).unwrap();
        assert_eq!(session.hit_test(3.0, 3.0), Some(outer));
    }

    #[test]
    fn scroll_offsets_clamp_and_hit_test_respects_overflow() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='scroll' style='overflow:auto;width:10px;height:10px'><div id='first' style='height:10px;background:red'></div><div id='second' style='height:10px;background:blue'></div></div>", 16).unwrap();
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
        let port = session.scrollport(container).unwrap();
        assert_eq!(
            (port.x, port.y, port.width, port.height),
            (0.0, 0.0, 10.0, 10.0)
        );
        let root_port = session.scrollport(session.document().root()).unwrap();
        assert_eq!(
            (root_port.x, root_port.y, root_port.width, root_port.height),
            (0.0, 0.0, 30.0, 30.0)
        );
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
    fn scrollport_uses_padding_box_and_owner_transform_chain() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='scroll' style='overflow:auto;width:10px;height:10px;padding:2px 3px;transform:translate(20px,10px)'><div style='height:30px'></div></div></body>",
            16,
        )
        .unwrap();
        let scroll = crate::selector::query_selector(&document, document.root(), "#scroll")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(60, 60, &NoText).unwrap();
        let port = session.scrollport(scroll).unwrap();
        assert_eq!(
            (port.x, port.y, port.width, port.height),
            (20.0, 10.0, 16.0, 14.0)
        );

        session.set_scroll_offset(scroll, 0.0, 5.0).unwrap();
        session.display_list(60, 60, &NoText).unwrap();
        assert_eq!(session.scrollport(scroll), Some(port));
    }

    #[test]
    fn nested_scrollport_moves_on_fast_outer_scroll_and_matches_relayout() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='outer' style='overflow:auto;width:20px;height:20px'><div style='height:40px'></div><div id='inner' style='overflow:auto;width:10px;height:10px;transform:translateX(2px)'><div style='height:20px'></div></div></div></body>",
            16,
        )
        .unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let outer = find("#outer");
        let inner = find("#inner");
        let mut session = RenderSession::new(document);
        session.display_list(60, 60, &NoText).unwrap();
        assert!(session
            .cached
            .as_ref()
            .unwrap()
            .geometry
            .scroll_regions
            .iter()
            .any(|region| region.node == outer));
        let outer_port = session.scrollport(outer).unwrap();
        let initial_inner = session.scrollport(inner).unwrap();
        assert_eq!((initial_inner.x, initial_inner.y), (2.0, 40.0));

        session.set_scroll_offset(outer, 0.0, 10.0).unwrap();
        let fast_inner = session.scrollport(inner).unwrap();
        assert_eq!((fast_inner.x, fast_inner.y), (2.0, 30.0));
        assert_eq!(session.scrollport(outer), Some(outer_port));

        // Force the same scroll state through a full layout and compare the
        // retained geometry against the translation-only fast path.
        session.cached = None;
        session.display_list(60, 60, &NoText).unwrap();
        assert_eq!(session.scrollport(inner), Some(fast_inner));
        assert_eq!(session.scrollport(outer), Some(outer_port));
    }

    #[test]
    fn root_scroll_extent_includes_visible_and_absolute_overflow_but_not_clipped_or_fixed() {
        let document = crate::html::parse(
            "<body style='margin:0'><div id='flow' style='width:60px;height:80px'></div><div id='clipped' style='width:10px;height:10px;overflow:hidden'><div style='width:200px;height:200px'></div></div><div id='absolute' style='position:absolute;left:80px;top:0;width:10px;height:10px'></div><div id='fixed' style='position:fixed;left:2px;top:3px;width:5px;height:5px'></div><div style='position:fixed;left:300px;top:300px;width:10px;height:10px'></div></body>",
            16,
        )
        .unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let flow = find("#flow");
        let fixed = find("#fixed");
        let absolute = find("#absolute");
        let mut session = RenderSession::new(document);
        session.display_list(40, 30, &NoText).unwrap();
        assert_eq!(
            session.scroll_extent(session.document().root()),
            Some((50.0, 60.0))
        );
        let fixed_before = session.hit_bounds(fixed).unwrap();
        assert_eq!((fixed_before.x, fixed_before.y), (2.0, 3.0));
        assert!(session.is_viewport_fixed(fixed));
        assert!(!session.is_viewport_fixed(flow));

        session
            .document_mut()
            .set_attribute(absolute, "style", "display:none")
            .unwrap();
        session.display_list(40, 30, &NoText).unwrap();
        assert_eq!(
            session.scroll_extent(session.document().root()),
            Some((20.0, 60.0))
        );

        session
            .set_scroll_offset(session.document().root(), 0.0, 20.0)
            .unwrap();
        session.display_list(40, 30, &NoText).unwrap();
        assert_eq!(session.hit_bounds(flow).unwrap().y, -20.0);
        let fixed_after = session.hit_bounds(fixed).unwrap();
        assert_eq!((fixed_after.x, fixed_after.y), (2.0, 3.0));
        assert_eq!(
            session
                .scrollport(session.document().root())
                .unwrap()
                .height,
            30.0
        );
    }

    #[test]
    fn transformed_hits_follow_nested_rotation_clipping_and_scroll() {
        let document = crate::html::parse("<body style='margin:0'><div id='outer' style='width:20px;height:20px;transform:translate(20px,10px);overflow:hidden;border-radius:5px'><div id='inner' style='width:20px;height:20px;transform:rotate(90deg);background:red'></div></div>",16).unwrap();
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

        let document=crate::html::parse("<body style='margin:0'><div id='scroll' style='width:10px;height:10px;overflow:auto;transform:translate(20px,10px)'><div style='height:10px'></div><div id='second' style='height:10px;transform:scale(1);background:red'></div></div>",16).unwrap();
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
        let document=crate::html::parse("<body style='margin:0'><div style='width:20px;height:20px;margin-left:100px;transform:translateX(-90px)'><div id='fixed' style='position:fixed;left:0;top:0;width:5px;height:5px;background:red'></div></div>",16).unwrap();
        let fixed = crate::selector::query_selector(&document, document.root(), "#fixed")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let list = session.display_list(40, 40, &NoText).unwrap();
        assert!(list.0.iter().any(|command|matches!(command,crate::paint::Command::FillRect{color,..} if color.r==255 && color.g==0)));
        assert!(!session.is_viewport_fixed(fixed));
        assert_eq!(session.hit_test(12.0, 2.0), Some(fixed));
        assert_ne!(session.hit_test(2.0, 2.0), Some(fixed));
    }

    #[test]
    fn transformed_flex_stretch_uses_final_origin_and_singular_boxes_do_not_hit() {
        let document=crate::html::parse("<body style='margin:0'><div style='display:flex;width:40px;height:20px'><div id='item' style='width:10px;transform:rotate(90deg);background:red'></div></div><div id='singular' style='width:10px;height:10px;transform:scale(0);background:blue'></div>",16).unwrap();
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
                glyphs: Arc::from([]),
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
        let document = crate::html::parse(
            "<body style='margin:0'><div style='background:red;width:2px;height:1px'></div>",
            8,
        )
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

    #[test]
    fn animation_overlay_uses_css_cascade_without_dom_mutation() {
        let document = crate::html::parse(
            "<style>#target { opacity: 0.9 !important }</style><div id='target' style='opacity: 0.2'></div><div id='animated' style='opacity: 0.2'></div>",
            16,
        ).unwrap();
        let target = crate::selector::query_selector(&document, document.root(), "#target")
            .unwrap()
            .unwrap();
        let animated = crate::selector::query_selector(&document, document.root(), "#animated")
            .unwrap()
            .unwrap();
        let mut session = RenderSession::new(document);
        let version = session.document().version();
        assert_eq!(session.computed_style(target).unwrap().opacity, 0.9);
        session
            .set_animation_declarations(target, alloc::vec![("opacity".into(), "0.7".into())])
            .unwrap();
        assert_eq!(session.document().version(), version);
        assert_eq!(session.computed_style(target).unwrap().opacity, 0.9);
        session
            .set_animation_declarations(target, alloc::vec![("opacity".into(), "0.4".into())])
            .unwrap();
        assert_eq!(session.document().version(), version);
        assert_eq!(session.computed_style(target).unwrap().opacity, 0.9);
        let style = match session.document().kind(target).unwrap() {
            NodeKind::Element { attributes, .. } => attributes
                .iter()
                .find(|(name, _)| name == "style")
                .unwrap()
                .1
                .clone(),
            _ => unreachable!(),
        };
        assert_eq!(style, "opacity: 0.2");
        session
            .set_animation_declarations(target, Vec::new())
            .unwrap();
        assert_eq!(session.computed_style(target).unwrap().opacity, 0.9);
        session
            .set_animation_declarations(animated, alloc::vec![("opacity".into(), "0.75".into())])
            .unwrap();
        assert_eq!(session.document().version(), version);
        assert_eq!(session.computed_style(animated).unwrap().opacity, 0.75);
        session
            .set_animation_declarations(animated, Vec::new())
            .unwrap();
        assert_eq!(session.computed_style(animated).unwrap().opacity, 0.2);
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
    fn node_bitmap_resolver_forwards_external_image_generation() {
        let images = Images {
            generation: Cell::new(23),
            ready: Cell::new(false),
            calls: Cell::new(0),
            image: Arc::new(ImageData {
                width: 1,
                height: 1,
                pixels: alloc::vec![0, 0, 0, 0],
            }),
        };
        let node_images = NodeImages {
            bitmaps: &[],
            fallback: Some(&images),
        };
        assert_eq!(ImageResolver::generation(&node_images), 23);
    }

    #[test]
    fn image_generation_controls_retained_display_list() {
        let document = crate::html::parse(
            "<body style='margin:0'><img src='tile.png' width='1' height='1'>",
            8,
        )
        .unwrap();
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

    #[test]
    fn external_image_updates_invalidate_layout_and_paint() {
        let document = crate::html::parse("<body></body>", 8).unwrap();
        let mut session = RenderSession::new(document);
        let frame = session.frame_id();
        let revision = session.paint_revision();
        session.invalidate_image_resources();
        assert_eq!(session.frame_id(), frame.wrapping_add(1));
        assert_eq!(session.paint_revision(), revision.wrapping_add(1));
        assert!(session.cached_display_list().is_none());
    }
}
