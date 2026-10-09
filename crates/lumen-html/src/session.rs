//! Retained document paint state. Scale-only changes replay the same CSS display list.
use crate::{
    css::{self, Style, StyleIndex},
    layout::{self, ImageResolver, LayoutError},
    paint::{Affine, DisplayList, ImageData, Rect, Rgba, TextShaper},
    Document, MutationKind, NodeId, NodeKind,
};
use alloc::{boxed::Box, rc::{Rc, Weak}, string::String, sync::Arc, vec::Vec};
use core::cell::{Cell, RefCell};

fn apply_paint_worklet_cache(commands: &mut [crate::paint::Command],
    cache: &[(crate::paint::PaintWorkletRequest, Arc<crate::render_capture::ReservedImageData>)],
    requests: &mut Vec<crate::paint::PaintWorkletRequest>) {
    fn paint(image: &mut crate::paint::BackgroundPaint,
        cache: &[(crate::paint::PaintWorkletRequest, Arc<crate::render_capture::ReservedImageData>)],
        requests: &mut Vec<crate::paint::PaintWorkletRequest>, depth: usize) {
        if depth > 16 { return; }
        match image {
            crate::paint::BackgroundPaint::Worklet(image) => {
                if requests.len() < 256 && !requests.contains(&image.request) { requests.push(image.request.clone()); }
                let pixels = cache.iter().find(|(request,_)|*request==image.request).map(|(_,pixels)|pixels.clone());
                Arc::make_mut(image).pixels = pixels;
            }
            crate::paint::BackgroundPaint::Border(border)=>paint(&mut Arc::make_mut(border).image,cache,requests,depth+1),
            crate::paint::BackgroundPaint::CrossFade(images) => {
                for (image,_) in Arc::make_mut(images) { paint(image, cache, requests, depth+1); }
            }
            _ => {}
        }
    }
    fn command(value: &mut crate::paint::Command,
        cache: &[(crate::paint::PaintWorkletRequest, Arc<crate::render_capture::ReservedImageData>)],
        requests: &mut Vec<crate::paint::PaintWorkletRequest>, depth: usize) {
        if depth > 32 { return; }
        match value {
            crate::paint::Command::FillBackground(fill) => paint(&mut fill.image, cache, requests, 0),
            crate::paint::Command::MaskedBackground(mask) => command(&mut mask.paint, cache, requests, depth+1),
            _ => {}
        }
    }
    for value in commands { command(value, cache, requests, 0); }
}

fn import_source_mut<'a>(source: &'a mut css::StylesheetSource, path: &[usize]) -> Option<&'a mut css::StylesheetSource> {
    let mut source = source;
    for &index in path { source = source.imports.get_mut(index)?.source.as_deref_mut()?; }
    Some(source)
}

fn import_source_ref<'a>(source: &'a css::StylesheetSource, path: &[usize]) -> Option<&'a css::StylesheetSource> {
    let mut source = source;
    for &index in path { source = source.imports.get(index)?.source.as_deref()?; }
    Some(source)
}

fn import_rule_ref<'a>(source:&'a css::StylesheetSource,path:&[usize])->Option<&'a css::LoadedImport> {
    let (index,parent)=path.split_last()?;import_source_ref(source,parent)?.imports.get(*index)
}
fn import_rule_mut<'a>(source:&'a mut css::StylesheetSource,path:&[usize])->Option<&'a mut css::LoadedImport> {
    let (index,parent)=path.split_last()?;import_source_mut(source,parent)?.imports.get_mut(*index)
}

#[derive(Clone,Debug,PartialEq)]
pub struct AdoptedStylesheetSource {
    pub text: String,
    pub base_url: Option<Arc<str>>,
    pub media: Arc<str>,
    pub disabled: bool,
}
impl From<String> for AdoptedStylesheetSource {
    fn from(text:String)->Self {Self{text,base_url:None,media:Arc::from(""),disabled:false}}
}

pub struct StylesheetGraphLease {
    _epoch_token: Rc<Cell<u64>>,
    text: RefCell<Arc<str>>,
    detached: Cell<bool>,
    disabled: Cell<bool>,
    source: RefCell<Option<alloc::boxed::Box<css::StylesheetSource>>>,
    imports: RefCell<Vec<Weak<StylesheetImportLease>>>,
    metadata: Option<Rc<layout::StylesheetMetadata>>,
}

/// Rare native import owners share a moved graph after its DOM source expires.
/// The core retains only weak leases; no JS values or callbacks enter Session.
pub struct StylesheetImportLease {
    graph: Rc<StylesheetGraphLease>,
    path: RefCell<Option<Vec<usize>>>,
}

impl StylesheetImportLease {
    /// The occurrence follows insert/delete rebasing, independently of its URL.
    /// A removed occurrence or a replaced owning sheet has no live path.
    pub fn with_live_path<R>(&self,read:impl FnOnce(Option<&[usize]>)->R)->R {
        let path=self.path.borrow();
        read(if self.graph.detached.get() {None}else{path.as_deref()})
    }

    pub fn is_detached(&self) -> bool { self.graph.detached.get() }
    pub fn with_detached_source<R>(&self, read: impl FnOnce(Option<&css::StylesheetSource>) -> R) -> Option<R> {
        let path = self.path.borrow();
        let path = path.as_deref()?;
        let source = self.graph.source.borrow();
        let root = source.as_deref()?;
        Some(read(import_source_ref(root, path)))
    }

    pub fn with_detached_source_mut<R>(&self, edit: impl FnOnce(Option<&mut css::StylesheetSource>) -> R) -> Option<R> {
        let path = self.path.borrow();
        let path = path.as_deref()?;
        let mut source = self.graph.source.borrow_mut();
        let root = source.as_deref_mut()?;
        Some(edit(import_source_mut(root, path)))
    }

    pub fn rebase_imports(&self, mapping: &[Option<usize>]) {
        if let Some(path) = self.path.borrow().as_deref() { self.graph.rebase_imports(path, mapping); }
    }
}

impl StylesheetGraphLease {
    pub fn metadata(&self)->Option<&Rc<layout::StylesheetMetadata>> {self.metadata.as_ref()}

    pub fn is_detached(&self) -> bool { self.detached.get() }
    pub fn disabled(&self) -> bool { self.disabled.get() }
    pub fn set_disabled(&self, disabled: bool) { self.disabled.set(disabled); }
    pub fn with_text<R>(&self, read: impl FnOnce(&str) -> R) -> R { read(&self.text.borrow()) }
    pub fn with_detached_source<R>(&self, read: impl FnOnce(Option<&css::StylesheetSource>) -> R) -> R {
        read(self.source.borrow().as_deref())
    }
    pub fn with_detached_source_mut<R>(&self, edit: impl FnOnce(Option<&mut css::StylesheetSource>) -> R) -> R {
        edit(self.source.borrow_mut().as_deref_mut())
    }
    fn rebase_imports(&self, prefix: &[usize], mapping: &[Option<usize>]) {
        let mut inverse = [usize::MAX; css::MAX_CSS_GRAPH_IMPORTS];
        for (next, previous) in mapping.iter().enumerate() {
            if let Some(previous) = previous { if let Some(slot) = inverse.get_mut(*previous) { *slot = next; } }
        }
        self.imports.borrow_mut().retain(|lease| lease.strong_count() != 0);
        for import in self.imports.borrow().iter().filter_map(Weak::upgrade) {
            let previous = {
                let path = import.path.borrow();
                let Some(path) = path.as_deref().filter(|path| path.len() > prefix.len() && path.starts_with(prefix)) else { continue; };
                path[prefix.len()]
            };
            let next = inverse.get(previous).copied().unwrap_or(usize::MAX);
            if next == usize::MAX { *import.path.borrow_mut() = None; }
            else if let Some(path) = import.path.borrow_mut().as_mut() { path[prefix.len()] = next; }
        }
    }
}

struct CachedFrame {
    document_version: u64,
    width: u32,
    height: u32,
    image_generation: Option<u64>,
    list: DisplayList,
    geometry: layout::LayoutGeometry,
}

struct NodeImages<'a> {
    color_schemes:css::ColorSchemeEnvironment,
    svg_fragment: Option<&'a crate::svg::Fragment>,
    svg_image_viewport:Option<(f32,f32)>,
    paint_definitions: &'a [(Arc<str>, Arc<[Arc<str>]>)],
    paint_registration_revision: u64,
    objects: &'a [(NodeId, crate::object::Representation, Option<crate::object::IntrinsicSize>)],
    bitmaps: &'a [(NodeId, Arc<ImageData>, crate::responsive_images::ImageMetadata)],
    embedded: &'a [(NodeId, Arc<crate::render_capture::ReservedImageData>)],
    fallback: Option<&'a dyn ImageResolver>,
}

impl ImageResolver for NodeImages<'_> {
    fn color_scheme_environment(&self)->css::ColorSchemeEnvironment {self.color_schemes}
    fn image_coordinate_scale(&self,image:&Arc<ImageData>,width:f32,height:f32,scheme:css::UsedColorScheme)->Option<[f32;2]> {self.fallback.and_then(|images|images.image_coordinate_scale(image,width,height,scheme))}
    fn paint_registration_revision(&self)->u64 {self.paint_registration_revision}
    fn svg_fragment(&self)->Option<&crate::svg::Fragment> {self.svg_fragment}
    fn svg_image_viewport(&self)->Option<(f32,f32)> {self.svg_image_viewport}
    fn resolve_viewport(&self,node:Option<NodeId>,base:&str,source:&str,natural:&Arc<crate::paint::ImageData>,width:f32,height:f32,scheme:css::UsedColorScheme)->Option<layout::ImageState> {
        let committed=node.and_then(|node|self.bitmaps.binary_search_by_key(&node.key(),|(id,_,_)|id.key()).ok()).and_then(|at|Arc::ptr_eq(&self.bitmaps[at].1,natural).then_some(&self.bitmaps[at].2)).and_then(|metadata|metadata.source.as_deref());
        self.fallback.and_then(|images|images.resolve_viewport(node,base,committed.unwrap_or(source),natural,width,height,scheme))
    }
    fn paint_input_properties(&self, name: &str) -> Option<&[Arc<str>]> {
        self.paint_definitions.iter().find(|(candidate,_)|candidate.as_ref()==name).map(|(_,inputs)|inputs.as_ref())
    }
    fn object_representation(&self, node: NodeId) -> crate::object::Representation {
        self.objects.binary_search_by_key(&node.key(), |(id, _, _)| id.key()).ok().map(|index| self.objects[index].1)
            .unwrap_or_else(|| self.fallback.map_or(crate::object::Representation::Fallback,
                |images| images.object_representation(node)))
    }

    fn image_intrinsic_size(&self,image:&Arc<ImageData>,scheme:css::UsedColorScheme)->Option<crate::object::IntrinsicSize> {
        self.fallback.and_then(|images|images.image_intrinsic_size(image,scheme))
    }
    fn node_image_intrinsic_size(&self,node:NodeId,image:&Arc<ImageData>,scheme:css::UsedColorScheme)->Option<crate::object::IntrinsicSize> {
        let owner=self.fallback.and_then(|images|images.node_image_intrinsic_size(node,image,scheme));
        let metadata=self.bitmaps.binary_search_by_key(&node.key(),|(node,_,_)|node.key()).ok()
            .and_then(|at|Arc::ptr_eq(&self.bitmaps[at].1,image).then_some(&self.bitmaps[at].2));
        if let Some(metadata)=metadata {
            let raw=owner.or(metadata.intrinsic).unwrap_or(crate::object::IntrinsicSize{width:Some(image.width as f32),height:Some(image.height as f32),ratio:(image.height!=0).then(||image.width as f32/image.height as f32)});
            raw.density_corrected(metadata.density)
        }else{owner}
    }
    fn embedded_document_image(&self,node:NodeId)->Option<Arc<crate::render_capture::ReservedImageData>> {
        self.embedded.binary_search_by_key(&node.key(),|(id,_)|id.key()).ok()
            .map(|at|self.embedded[at].1.clone())
            .or_else(||self.fallback.and_then(|images|images.embedded_document_image(node)))
    }
    fn embedded_intrinsic_size(&self, node: NodeId) -> Option<crate::object::IntrinsicSize> {
        self.objects.binary_search_by_key(&node.key(), |(id, _, _)| id.key()).ok()
            .and_then(|index| self.objects[index].2)
            .or_else(|| self.fallback.and_then(|images| images.embedded_intrinsic_size(node)))
    }
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
    fn resolve_image_request(&self,node:NodeId,base:&str,source:&str,parameters:layout::ImageRequestParameters)->layout::ImageState {
        self.fallback.map_or(layout::ImageState::Failed,|images|images.resolve_image_request(node,base,source,parameters))
    }
    fn node_origin_clean(&self, node: NodeId, base: &str, source: &str) -> bool {
        self.fallback
            .is_none_or(|images| images.node_origin_clean(node, base, source))
    }
    fn resolve_node(&self, node: NodeId) -> Option<layout::ImageState> {
        self.bitmaps.binary_search_by_key(&node.key(),|(id,_,_)|id.key()).ok()
            .map(|at|layout::ImageState::Ready(self.bitmaps[at].1.clone()))
            .or_else(|| self.fallback.and_then(|images| images.resolve_node(node)))
    }
    fn generation(&self) -> u64 {
        self.fallback
            .map_or(0, |images| ImageResolver::generation(images))
    }
}

// Canvas subtree snapshots never expose unreadable image pixels, including CSS images.
struct SnapshotImages<'a>(&'a dyn ImageResolver, &'a Document);
impl SnapshotImages<'_> {
    fn filtered(&self, state: layout::ImageState, readable: bool) -> layout::ImageState {
        if readable { return state; }
        match state {
            layout::ImageState::Ready(image) => layout::ImageState::Ready(Arc::new(ImageData {
                width: image.width, height: image.height, pixels: alloc::vec![0; image.pixels.len()],
            })),
            state => state,
        }
    }
}
impl ImageResolver for SnapshotImages<'_> {
    fn color_scheme_environment(&self)->css::ColorSchemeEnvironment {self.0.color_scheme_environment()}
    fn image_coordinate_scale(&self,image:&Arc<ImageData>,width:f32,height:f32,scheme:css::UsedColorScheme)->Option<[f32;2]> {self.0.image_coordinate_scale(image,width,height,scheme)}
    fn node_image_intrinsic_size(&self,node:NodeId,image:&Arc<ImageData>,scheme:css::UsedColorScheme)->Option<crate::object::IntrinsicSize> {self.0.node_image_intrinsic_size(node,image,scheme)}
    fn image_intrinsic_size(&self,image:&Arc<ImageData>,scheme:css::UsedColorScheme)->Option<crate::object::IntrinsicSize> {self.0.image_intrinsic_size(image,scheme)}
    fn paint_input_properties(&self,name:&str)->Option<&[Arc<str>]>{self.0.paint_input_properties(name)}
    fn paint_registration_revision(&self)->u64 {self.0.paint_registration_revision()}
    fn object_representation(&self,node:NodeId)->crate::object::Representation {self.0.object_representation(node)}
    fn svg_fragment(&self)->Option<&crate::svg::Fragment> {self.0.svg_fragment()}
    fn svg_image_viewport(&self)->Option<(f32,f32)> {self.0.svg_image_viewport()}
    fn resolve_viewport(&self,node:Option<NodeId>,base:&str,source:&str,natural:&Arc<crate::paint::ImageData>,width:f32,height:f32,scheme:css::UsedColorScheme)->Option<layout::ImageState> {self.0.resolve_viewport(node,base,source,natural,width,height,scheme)}
    fn embedded_intrinsic_size(&self,node:NodeId)->Option<crate::object::IntrinsicSize> { self.0.embedded_intrinsic_size(node) }
    fn resolve(&self,source:&str)->layout::ImageState {
        self.filtered(self.0.resolve(source), self.0.origin_clean_from("",source))
    }
    fn resolve_from(&self,base:&str,source:&str)->layout::ImageState {
        self.filtered(self.0.resolve_from(base,source), self.0.origin_clean_from(base,source))
    }
    fn resolve_node_from(&self,node:NodeId,base:&str,source:&str)->Option<layout::ImageState> {
        self.0.resolve_node_from(node,base,source).map(|state|self.filtered(state,self.0.node_origin_clean(node,base,source)))
    }
    fn resolve_image_request(&self,node:NodeId,base:&str,source:&str,parameters:layout::ImageRequestParameters)->layout::ImageState {
        self.filtered(self.0.resolve_image_request(node,base,source,parameters),self.0.node_origin_clean(node,base,source))
    }
    fn resolve_node(&self,node:NodeId)->Option<layout::ImageState> {
        let embeds_document = matches!(self.1.kind(node),Ok(NodeKind::Element{name,..}) if matches!(name.as_str(),"iframe"|"object"|"embed"));
        self.0.resolve_node(node).map(|state|self.filtered(state,!embeds_document))
    }
}

fn in_style(document: &Document, mut id: NodeId) -> bool {
    loop {
        match document.kind(id) {
            Ok(NodeKind::ProcessingInstruction { target, .. }) if target=="xml-stylesheet" => return true,
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

fn stylesheet_source_mutation_owner(document: &Document, mutation: &crate::Mutation) -> Option<NodeId> {
    match &mutation.kind {
        MutationKind::CharacterData | MutationKind::Tree { .. } => {
            if crate::xml_stylesheet::is_candidate(document,mutation.target) {return Some(mutation.target);}
            let mut node = mutation.target;
            while let Ok(kind) = document.kind(node) {
                if matches!(kind, NodeKind::Element { name, .. } if crate::svg::local_name(name) == "style") { return Some(node); }
                node = document.parent(node).ok().flatten()?;
            }
            None
        }
        MutationKind::Attribute(name) if name.eq_ignore_ascii_case("href") => {
            matches!(document.kind(mutation.target), Ok(NodeKind::Element { name, .. }) if crate::svg::local_name(name) == "link").then_some(mutation.target)
        }
        _ => None,
    }
}

pub(crate) fn subtree_has_style(document: &Document, root: NodeId) -> bool {
    let mut current = root;
    loop {
        match document.kind(current) {
            Ok(NodeKind::ProcessingInstruction { target, .. }) if target == "xml-stylesheet" => return true,
            Ok(NodeKind::Element { name, namespace, .. })
                if (*namespace == crate::Namespace::Html && matches!(crate::svg::local_name(name), "style" | "link"))
                    || (*namespace == crate::Namespace::Svg && crate::svg::local_name(name) == "style") => return true,
            Err(_) => return true,
            _ => {}
        }
        match crate::selector::next_shadow_including_descendant(document, root, current) {
            Ok(Some(next)) => current = next,
            Ok(None) => return false,
            Err(_) => return true,
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

pub type AnimationStyleInput = (NodeId, Option<css::PseudoElement>, Option<NodeId>, [Option<Arc<str>>; 19]);

/// Mutable host scroll samples. A batch uses one fresh layout's bounds even
/// when an earlier sample invalidates retained replay for the whole frame.
pub struct ScrollUpdate {
    pub node: NodeId,
    pub x: f32,
    pub y: f32,
    pub changed: bool,
    admitted: bool,
    order: usize,
}

impl ScrollUpdate {
    pub fn new(node: NodeId, x: f32, y: f32) -> Self {
        Self { node, x, y, changed: false, admitted: false, order: 0 }
    }
    pub fn has_scroll_box(&self) -> bool { self.admitted }
}

/// Immutable CSS animation inputs, shared across rendering opportunities.
pub struct AnimationSnapshot {
    pub generation: u64,
    pub nodes: Vec<AnimationStyleInput>,
    pub keyframes: Vec<css::KeyframesRuleText>,
    source: alloc::sync::Weak<TransitionSnapshot>,
    epoch: TransitionInputEpoch,
}

/// Inputs that cause a CSS style-change event. Sampling effects does not create
/// a new event, so this identity deliberately excludes the overlay epoch.
#[derive(Clone,Copy,PartialEq)]
pub struct TransitionInputEpoch {
    document_version:u64,
    rules_generation:u64,
    font_generation:u64,
    validity_generation:u64,
    interaction_generation:u64,
    environment:css::MediaEnvironment,
}

pub struct TransitionStyleInput {
    pub node:NodeId,
    pub parent:Option<NodeId>,
    pub pseudo:Option<css::PseudoElement>,
    pub style:Arc<Style>,
    rendered:Cell<bool>,
    pub eligibility:css::EffectEligibility,
}

impl TransitionStyleInput {
    pub fn was_rendered(&self)->bool {self.rendered.get()}
    pub fn set_rendered(&self,rendered:bool) {self.rendered.set(rendered);}
}

// One operation-local target closure; the ordered nodes preserve parent-before-child
// computation while the bits select only actual requested pseudo snapshots.
struct EffectStyleSelection {
    nodes:Vec<NodeId>,
    selected:alloc::collections::BTreeMap<u128,u16>,
    retained_bytes:usize,
}

pub struct TransitionSnapshot {
    pub epoch:TransitionInputEpoch,
    pub nodes:Vec<TransitionStyleInput>,
}

pub struct RenderSession {
    document: Document,
    rules: Option<StyleIndex>,
    font_face_rules: Vec<css::FontFaceRule>,
    font_face_generation: u64,
    font_face_descriptor_overrides: Vec<(css::FontFaceIdentity, css::FontFaceDescriptors)>,
    rules_version: u64,
    stylesheet_mutation_version: u64,
    stylesheet_owned_write: Option<(NodeId, u64)>,
    stylesheet_change_tokens: Vec<(NodeId, Weak<Cell<u64>>)>,
    stylesheet_graph_leases: Vec<(NodeId, u64, Weak<StylesheetGraphLease>)>,
    cssom_stylesheet_texts: Vec<layout::CssomStylesheetText>,
    cached: Option<CachedFrame>,
    rendered_text_requested: bool,
    frame_id: u64,
    font_generation: u64,
    validity_generation: u64,
    interaction_generation: u64,
    auto_directionality_scan: Option<(u64, bool)>,
    node_bitmaps: Vec<(NodeId, Arc<ImageData>, crate::responsive_images::ImageMetadata)>,
    embedded_bitmaps: Vec<(NodeId, Arc<crate::render_capture::ReservedImageData>)>,
    object_representations: Vec<(NodeId, crate::object::Representation, Option<crate::object::IntrinsicSize>)>,
    svg_fragment: Option<Box<crate::svg::Fragment>>,
    svg_image_viewport:Option<Box<(f32,f32)>>,
    paint_revision: u64,
    capture_overlay: Option<DisplayList>,
    paint_definitions: Vec<(Arc<str>, Arc<[Arc<str>]>)>,
    paint_worklet_cache: Vec<(crate::paint::PaintWorkletRequest, Arc<crate::render_capture::ReservedImageData>)>,
    canvas_background: Option<Rgba>,
    canvas_background_is_default: bool,
    embedding_color_scheme: Option<css::UsedColorScheme>,
    scrolls: Vec<layout::ScrollOffset>,
    normalized_scroll_events: Vec<NodeId>,
    adopted_stylesheets: Vec<(Option<NodeId>, Vec<AdoptedStylesheetSource>)>,
    cssom_rule_overrides: Vec<css::RuleDeclarationOverride>,
    cssom_topology_overrides: Vec<css::RuleTopologyOverride>,
    linked_stylesheets: Vec<(NodeId, String, String)>,
    stylesheet_sources: Vec<layout::LoadedStylesheet>,
    disabled_stylesheets: Vec<NodeId>,
    preferred_stylesheet_set: Option<Arc<str>>,
    stylesheet_metadata_budget:Option<Rc<Cell<usize>>>,
    document_base_url: Option<Arc<str>>,
    animated_styles: Vec<(NodeId, Option<css::PseudoElement>, css::EffectOrigin, Vec<(String, String)>)>,
    style_cache: RefCell<css::StyleCache>,
    layout_cache: layout::RetainedLayoutCache,
    style_version: u64,
    style_environment: Option<css::MediaEnvironment>,
    presentation_root: Option<NodeId>,
    animation_snapshot: Option<Arc<AnimationSnapshot>>,
    timeline_transform_values:Option<Arc<crate::animation::progress_timelines::TransformTimelineValues>>,
    forced_layout:bool,
    timeline_layout_stale:bool,
    // Only deterministic provenance-budget rejection is memoized. The rare
    // record has no DOM roots and includes actual font and paint inputs.
    rejected_transition_snapshot: Option<Box<(TransitionInputEpoch, Option<u64>, u64)>>,
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
    text_generation: Option<u64>,
    document_version: u64,
    rules_generation: u64,
    validity_generation: u64,
    interaction_generation: u64,
    animation_epoch: u64,
    environment: css::MediaEnvironment,
    layout_frame_id: u64,
}

pub(crate) fn style_with_query_context(
    document: &Document,
    node: NodeId,
    rules: &StyleIndex,
    cache: &mut css::StyleCache,
    geometry: Option<&layout::LayoutGeometry>,
    text:Option<&dyn TextShaper>,
    transition:bool,
) -> Result<(Style, css::ContainerUnitContext), LayoutError> {
    if node == document.root() {
        let mut context = css::ContainerUnitContext {
            width: css::ContainerUnitBasis::NoContainer,
            height: css::ContainerUnitBasis::NoContainer,
            inline: css::ContainerUnitBasis::NoContainer,
            block: css::ContainerUnitBasis::NoContainer,
            small_viewport: rules.environment,
            writing_mode: css::WritingMode::HorizontalTb,
        };
        let Some(root) = crate::selector::document_element(document) else {
            return Ok((Style::initial(), context));
        };
        let mut style = if transition { (*rules.compute_transition_style_cached(document,root,None,None,text,cache,false).map_err(LayoutError::Css)?).clone() } else { match text {Some(text)=>css::compute_node_cached_with_text(document,root,None,rules,cache,text),None=>css::compute_node_cached(document,root,None,rules,cache)}.map_err(LayoutError::Css)? };
        context = context.for_writing_mode(style.writing_mode);
        let _ = style.resolve_query_context_with_text(context,text);
        let _ = css::resolve_font_style_query_context(&mut style, context);
        return Ok((style, context));
    }
    let mut ancestors = Vec::new();
    let mut current = Some(node);
    while let Some(id) = current {
        if matches!(document.kind(id), Ok(NodeKind::Element { .. })) {
            if ancestors.len() >= 512 {
                return Err(LayoutError::DepthLimit);
            }
            ancestors
                .try_reserve(1)
                .map_err(|_| LayoutError::CommandLimit)?;
            ancestors.push(id);
        }
        current = document
            .flat_tree_parent(id)
            .map_err(|_| LayoutError::InvalidTree)?;
    }

    let mut context = css::ContainerUnitContext {
        width: css::ContainerUnitBasis::NoContainer,
        height: css::ContainerUnitBasis::NoContainer,
        inline: css::ContainerUnitBasis::NoContainer,
        block: css::ContainerUnitBasis::NoContainer,
        small_viewport: rules.environment,
        writing_mode: css::WritingMode::HorizontalTb,
    };
    let mut parent_style = None;
    let mut target_style = None;
    let mut inherited_pending = false;
    for id in ancestors.into_iter().rev() {
        let mut style=if transition { (*rules.compute_transition_style_cached(document,id,parent_style.as_deref(),None,text,cache,false).map_err(LayoutError::Css)?).clone() } else { match text {Some(text)=>css::compute_node_cached_with_text(document,id,parent_style.as_deref(),rules,cache,text),None=>css::compute_node_cached(document,id,parent_style.as_deref(),rules,cache)}.map_err(LayoutError::Css)? };
        context = context.for_writing_mode(style.writing_mode);
        let _ = style.resolve_query_context_with_text(context,text);
        let _ = css::resolve_font_style_query_context(&mut style, context);
        inherited_pending |= style.has_inherited_query_container_dependencies();
        // A raw cached snapshot must not replace the just-resolved result. If
        // an ancestor remains unmeasured, do not cache a child that would lose
        // its inherited dependency when the scalar fallback is copied.
        let resolved = if inherited_pending {
            cache.forget_snapshot_style(id);
            Arc::new(style)
        } else {
            cache.replace_snapshot_style(document, id, style)
        };
        if id == node {
            target_style = Some((*resolved).clone());
        } else if let Some(measured) = geometry.and_then(|geometry| geometry.container_context_for(id)) {
            context = measured;
        } else {
            context = layout::mark_unmeasured_container_units(context, &resolved);
        }
        parent_style = Some(resolved);
    }
    target_style
        .map(|style| (style, context))
        .ok_or(LayoutError::InvalidTree)
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
    /// Existing source epoch snapshot; callers obtain computed styles first.
    /// Sharing this Arc does not copy registration records for animation targets.
    pub fn registered_custom_property_snapshot(&self)->Option<Arc<[css::registered_properties::RegisteredCustomProperty]>> {
        self.rules.as_ref().map(|rules|rules.registered_snapshot())
    }
    pub fn registered_custom_property_syntax(&self,name:&str)->Option<String> {
        if let Some(rules) = &self.rules {
            return rules.registered_snapshot().iter().find(|property| property.name == name).map(|property| property.syntax.clone());
        }
        self.document.registered_custom_properties.iter().find(|property|property.name==name).map(|property|property.syntax.clone())
    }
    pub fn register_paint_worklet(&mut self, name: Arc<str>, inputs: Arc<[Arc<str>]>) -> Result<(),LayoutError> {
        if inputs.len()>256 || self.paint_definitions.len()>=1024 { return Err(LayoutError::CommandLimit); }
        if let Some((_,previous))=self.paint_definitions.iter_mut().find(|(candidate,_)|*candidate==name) { *previous=inputs; }
        else { self.paint_definitions.push((name,inputs)); }
        self.invalidate_paint();
        Ok(())
    }

    pub fn paint_worklet_requests(&self) -> Vec<crate::paint::PaintWorkletRequest> {
        let Some(frame)=&self.cached else { return Vec::new(); };
        let mut commands=frame.list.0.clone();
        let mut requests=Vec::new();
        apply_paint_worklet_cache(&mut commands,&[],&mut requests);
        requests.retain(|request|!self.paint_worklet_cache.iter().any(|(candidate,_)|candidate==request));
        requests
    }

    pub fn complete_paint_worklets(&mut self, images: Vec<(crate::paint::PaintWorkletRequest,
        Arc<crate::render_capture::ReservedImageData>)>) -> Result<(),LayoutError> {
        for (request,pixels) in images {
            if let Some((_,old))=self.paint_worklet_cache.iter_mut().find(|(candidate,_)|*candidate==request) { *old=pixels; }
            else {
                if self.paint_worklet_cache.len()>=256 { return Err(LayoutError::CommandLimit); }
                self.paint_worklet_cache.push((request,pixels));
            }
        }
        if let Some(frame)=&mut self.cached {
            let mut requests=Vec::new();
            apply_paint_worklet_cache(&mut frame.list.0,&self.paint_worklet_cache,&mut requests);
        }
        self.paint_revision=self.paint_revision.wrapping_add(1);
        Ok(())
    }

    pub fn clear_paint_worklets(&mut self) {
        self.paint_definitions.clear();self.paint_worklet_cache.clear();self.invalidate_paint();
    }
    pub fn new(document: Document) -> Self {
        let interaction_generation = document.interaction_generation();
        let stylesheet_mutation_version = document.version();
        Self {
            document,
            rules: None,
            font_face_rules: Vec::new(),
            font_face_generation: 0,
            font_face_descriptor_overrides: Vec::new(),
            rules_version: 0,
            stylesheet_mutation_version,
            stylesheet_owned_write: None,
            stylesheet_change_tokens: Vec::new(),
            stylesheet_graph_leases: Vec::new(),
            cssom_stylesheet_texts: Vec::new(),
            cached: None,
            rendered_text_requested: false,
            frame_id: 0,
            font_generation: 0,
            validity_generation: 0,
            interaction_generation,
            auto_directionality_scan: None,
            node_bitmaps: Vec::new(),
            embedded_bitmaps:Vec::new(),
            object_representations: Vec::new(),
            svg_fragment: None,
            svg_image_viewport:None,
            paint_revision: 0,
            paint_definitions: Vec::new(),
            paint_worklet_cache: Vec::new(),
            capture_overlay: None,
            canvas_background: Some(layout::DEFAULT_CANVAS_BACKGROUND),
            canvas_background_is_default: true,
            embedding_color_scheme: None,
            scrolls: Vec::new(),
            normalized_scroll_events: Vec::new(),
            adopted_stylesheets: Vec::new(),
            cssom_rule_overrides: Vec::new(),
            cssom_topology_overrides: Vec::new(),
            linked_stylesheets: Vec::new(),
            stylesheet_sources: Vec::new(),
            disabled_stylesheets: Vec::new(),
            preferred_stylesheet_set: None,
            stylesheet_metadata_budget:None,
            document_base_url: None,
            animated_styles: Vec::new(),
            style_cache: RefCell::default(),
            layout_cache: layout::RetainedLayoutCache::default(),
            style_version: 0,
            style_environment: None,
            presentation_root: None,
            animation_snapshot: None,
            timeline_transform_values:None,
            forced_layout:false,
            timeline_layout_stale:false,
            rejected_transition_snapshot: None,
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

    pub fn set_highlights(&mut self, mut highlights: Vec<crate::highlights::Highlight>) {
        highlights.sort_by_key(|highlight| (highlight.priority, highlight.order));
        if self.document.highlights != highlights {
            self.document.highlights = highlights;
            self.invalidate_paint();
        }
    }

    pub fn highlights(&self) -> &[crate::highlights::Highlight] {
        &self.document.highlights
    }

    pub fn register_custom_property(&mut self, name: String, syntax: &str, inherits: bool,
        initial_value: Option<String>) -> Result<(), css::registered_properties::RegistrationError> {
        css::registered_properties::register(&mut self.document.registered_custom_properties,
            name, syntax, inherits, initial_value)?;
        if let Some(property)=self.document.registered_custom_properties.last_mut() {property.source_url=self.document_base_url.clone();}
        if let Some(rules) = &mut self.rules { rules.set_registered_properties(&self.document.registered_custom_properties); }
        self.rules_generation = self.rules_generation.wrapping_add(1);
        self.style_cache.borrow_mut().clear();
        self.read_style_cache.clear();
        self.read_style_key = None;
        self.animation_snapshot = None;
        self.pending_animation_full = true;
        self.invalidate_paint();
        Ok(())
    }

    /// Choose the backing canvas color; image documents use `None` for transparency.
    pub fn set_canvas_background(&mut self, background: Option<Rgba>) {
        if self.canvas_background_is_default || self.canvas_background != background {
            self.canvas_background_is_default=false;
            self.canvas_background = background;
            // Backing canvas paint precedes every retained box fragment and
            // does not change its style, geometry, or propagated CSS canvas
            // source. Rebuild the full list without discarding those boxes.
            self.cached = None;
            self.paint_revision = self.paint_revision.wrapping_add(1);
        }
    }

    /// Publish the element algorithm's actual representation. Pending/document
    /// objects suppress fallback children; failed/empty objects restore them.
    pub fn set_object_representation(&mut self, node: NodeId,
        value: crate::object::Representation) -> Result<(), LayoutError> {
        let kind=crate::object::kind(&self.document,node).ok_or(LayoutError::InvalidTree)?;
        if kind==crate::object::Kind::Embed && matches!(value,crate::object::Representation::Fallback|crate::object::Representation::Image)
            || kind==crate::object::Kind::Object && value==crate::object::Representation::Nothing {return Err(LayoutError::InvalidTree);}
        let inactive=kind.inactive();
        let index = self.object_representations.binary_search_by_key(&node.key(), |(id, _, _)| id.key());
        let previous = index.ok().map_or(inactive,
            |index| self.object_representations[index].1);
        if previous == value { return Ok(()) }
        match index {
            Ok(index) if value == inactive => { self.object_representations.remove(index); }
            Ok(index) => { self.object_representations[index].1 = value; self.object_representations[index].2 = None; },
            Err(index) if value != inactive => {
                self.object_representations.try_reserve(1).map_err(|_| LayoutError::CommandLimit)?;
                self.object_representations.insert(index, (node, value, None));
            }
            Err(_) => {}
        }
        if value != crate::object::Representation::Image {
            self.node_bitmaps.retain(|(id, _, _)| *id != node);
        }
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Publish only natural metadata from the currently committed document. The
    /// metadata carries no DOM/native owners and lives with the rare representation.
    pub fn set_embedded_intrinsic_size(&mut self, node: NodeId, value: Option<crate::object::IntrinsicSize>) -> Result<(), LayoutError> {
        if value.is_some_and(|value| !value.is_valid()) { return Err(LayoutError::InvalidTree); }
        let index = self.object_representations.binary_search_by_key(&node.key(), |(id, _, _)| id.key())
            .map_err(|_| LayoutError::InvalidTree)?;
        if self.object_representations[index].1 != crate::object::Representation::Document { return Err(LayoutError::InvalidTree); }
        if self.object_representations[index].2 == value { return Ok(()); }
        self.object_representations[index].2 = value;
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Capture bounded URL view metadata; actual ID targets remain live and are
    /// resolved through the shared index at layout/natural-size computation.
    pub fn set_svg_image_viewport(&mut self,viewport:Option<(f32,f32)>)->Result<(),LayoutError> {
        if viewport.is_some_and(|(width,height)|!width.is_finite() || !height.is_finite() || width<=0.0 || height<=0.0) {return Err(LayoutError::InvalidTree);}
        if self.svg_image_viewport.as_deref().copied()!=viewport {
            self.svg_image_viewport=viewport.map(Box::new);
            self.invalidate_paint();
        }
        Ok(())
    }
    pub fn svg_fragment(&self)->Option<&crate::svg::Fragment> {self.svg_fragment.as_deref()}
    pub fn set_svg_fragment(&mut self,fragment:Option<&str>)->Result<(),LayoutError> {
        if fragment.is_some_and(|fragment|fragment.len()>8192) {
            if self.svg_fragment.take().is_some(){self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);}
            return Err(LayoutError::CommandLimit);
        }
        let decoded=fragment.map(|fragment|lumen_common::codec::percent_decode(fragment.as_bytes()));
        let value=decoded.as_deref().and_then(|bytes|crate::svg::Fragment::parse(&String::from_utf8_lossy(bytes)));
        if self.svg_fragment.as_deref()==value.as_ref(){return Ok(());}
        self.svg_fragment=value.map(Box::new);
        self.invalidate_paint();
        self.frame_id=self.frame_id.wrapping_add(1);
        Ok(())
    }
    pub fn embedded_document_is_svg(&self)->bool {
        self.document.document_element_at(self.document.root()).ok().flatten().is_some_and(|root|
            matches!(self.document.kind(root),Ok(NodeKind::Element {namespace:crate::Namespace::Svg,name,..}) if crate::svg::local_name(name)=="svg"))
    }

    /// No painting or used viewport measurement is involved in natural sizing.
    pub fn embedded_document_intrinsic_size(&mut self, raster: bool, text: Option<&dyn TextShaper>) -> Result<Option<crate::object::IntrinsicSize>, LayoutError> {
        if raster {
            return Ok(self.node_bitmaps.first().map(|(_, image, _)| crate::object::IntrinsicSize {
                width: Some(image.width as f32), height: Some(image.height as f32),
                ratio: (image.height != 0).then(|| image.width as f32 / image.height as f32),
            }));
        }
        let root = self.document.document_element_at(self.document.root()).map_err(|_|LayoutError::InvalidTree)?;
        let Some(root) = root else { return Ok(None); };
        let view_box = match self.document.kind(root) {
            Ok(NodeKind::Element { namespace: crate::Namespace::Svg, name, attributes }) if crate::svg::local_name(name) == "svg" =>
                crate::svg::attribute(attributes,"viewBox").and_then(crate::svg::parse_view_box),
            _ => return Ok(None),
        };
        let view_box=self.svg_fragment.as_deref().and_then(|fragment|fragment.resolve(&self.document)).and_then(|view|view.view_box).or(view_box);
        let style = self.computed_style_with_text(root,text)?;
        let width = style.intrinsic_dimension(true);
        let height = style.intrinsic_dimension(false);
        let ratio = match (width,height) {
            (Some(width),Some(height)) if width > 0.0 && height > 0.0 => Some(width / height),
            (Some(_),Some(_)) => None,
        _ => view_box.map(|view_box| view_box.width / view_box.height),
        };
        Ok(Some(crate::object::IntrinsicSize { width, height, ratio: ratio.filter(|value|value.is_finite() && *value > 0.0) }))
    }

    /// Sparse host size publication shares the canonical style input identity;
    /// bitmap/effect changes also invalidate natural metadata without a tree scan.
    pub fn embedded_intrinsic_epoch(&mut self) -> Result<(TransitionInputEpoch,u64,u64),LayoutError> {
        Ok((self.transition_input_epoch()?,self.animation_epoch,self.paint_revision))
    }

    /// Pixel reservation follows the active child through retained paint commands.
    pub fn set_embedded_document_image(&mut self,node:NodeId,image:Option<Arc<crate::render_capture::ReservedImageData>>)->Result<bool,LayoutError> {
        if !matches!(self.document.kind(node),Ok(NodeKind::Element { namespace:crate::Namespace::Html,name,.. }) if matches!(name.as_str(),"iframe"|"object"|"embed")) {
            return Err(LayoutError::InvalidTree);
        }
        let at=self.embedded_bitmaps.binary_search_by_key(&node.key(),|(id,_)|id.key());
        match (at,image) {
            (Ok(at),Some(image)) if Arc::ptr_eq(&self.embedded_bitmaps[at].1,&image)=>return Ok(false),
            (Err(_),None)=>return Ok(false),
            (Ok(at),None)=>{self.embedded_bitmaps.remove(at);},
            (Ok(at),Some(image))=>self.embedded_bitmaps[at].1=image,
            (Err(at),Some(image))=>{
                self.embedded_bitmaps.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
                self.embedded_bitmaps.insert(at,(node,image));
            },
        }
        self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(true)
    }
    pub fn retain_embedded_document_images(&mut self,keep:impl Fn(NodeId)->bool)->bool {
        let before=self.embedded_bitmaps.len();
        self.embedded_bitmaps.retain(|(node,_)|self.document.kind(*node).is_ok() && keep(*node));
        let changed=before!=self.embedded_bitmaps.len();
        if changed {self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);}
        changed
    }

    pub fn set_node_bitmap(
        &mut self,
        node: NodeId,
        image: Option<Arc<ImageData>>,
    ) -> Result<(), LayoutError> {
        self.set_node_bitmap_with_intrinsic(node,image,None)
    }
    /// Publish dimensions with the same selected resource and physical bitmap.
    pub fn set_node_bitmap_with_intrinsic(&mut self,node:NodeId,image:Option<Arc<ImageData>>,intrinsic:Option<crate::object::IntrinsicSize>)->Result<(),LayoutError> {
        self.set_node_bitmap_with_metadata(node,image,crate::responsive_images::ImageMetadata{intrinsic,..Default::default()})
    }
    pub fn set_node_bitmap_with_metadata(&mut self,node:NodeId,image:Option<Arc<ImageData>>,metadata:crate::responsive_images::ImageMetadata)->Result<(),LayoutError> {
        if !metadata.is_valid() {return Err(LayoutError::ImageFailed);}
        // Image Button fetches retain their available image across type
        // changes. The canonical input owner may receive an in-flight result
        // while its current type is non-image; only image layout uses it.
        if !matches!(crate::forms::html_element_local_name(&self.document,node),Some("canvas"|"img"|"video"|"object"|"input")) {
            return Err(LayoutError::InvalidTree);
        }
        if image.as_ref().is_some_and(|image| !image.is_valid()) {
            return Err(LayoutError::ImageFailed);
        }
        let at=self.node_bitmaps.binary_search_by_key(&node.key(),|(id,_,_)|id.key());
        let previous=at.ok().map(|at|&self.node_bitmaps[at]);
        if match (previous,image.as_ref()) {
            (None,None)=>true,
            (Some((_,previous,old)),Some(image))=>Arc::ptr_eq(previous,image) && *old==metadata,
            _=>false,
        } {return Ok(());}
        match (at,image) {
            (Ok(at),Some(image))=>self.node_bitmaps[at]=(node,image,metadata),
            (Ok(at),None)=>{self.node_bitmaps.remove(at);},
            (Err(at),Some(image))=>{self.node_bitmaps.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;self.node_bitmaps.insert(at,(node,image,metadata));},
            (Err(_),None)=>{},
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
        // Live value changes affect control ink and often intrinsic geometry
        // even when no stylesheet contains a form-state selector. Keep the
        // existing generation as the sole invalidation authority.
        let live_value_changed = validity_changed && self.document.has_form_value_resolver();
        if live_value_changed {
            self.invalidate_paint();
            self.frame_id = self.frame_id.wrapping_add(1);
        }
        if !has_selector_dependencies
            && !(validity_changed && self.has_auto_directionality_controls())
        {
            return;
        }
        self.style_cache.borrow_mut().clear();
        self.animation_snapshot = None;
        if !live_value_changed {
            self.invalidate_paint();
            self.frame_id = self.frame_id.wrapping_add(1);
        }
    }
    pub fn style_cache_stats(&self) -> css::StyleCacheStats {
        self.style_cache.borrow().stats()
    }
    pub fn read_style_cache_stats(&self)->css::StyleCacheStats {
        self.read_style_cache.stats()
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
        self.reclaim_stale_stylesheet_sources();
        let previous_disabled = self.stylesheet_disabled(node);
        let source_index = self.stylesheet_sources.iter().position(|loaded| loaded.owner == node);
        let identity = layout::stylesheet_identity(&self.document,node)?
            .or_else(||source.is_none().then(||source_index.map(|index|self.stylesheet_sources[index].identity.clone())).flatten())
            .ok_or(LayoutError::InvalidTree)?;
        let linked_unchanged = match &identity {
            layout::StylesheetIdentity::Inline(_) => true,
            layout::StylesheetIdentity::Link(href) => match &source {
                Some(source) => self.linked_stylesheets.iter().any(|(owner, current_href, text)| *owner == node && current_href == href && text == source.text.as_ref()),
                None => !self.linked_stylesheets.iter().any(|(owner, _, _)| *owner == node),
            },
        };
        if source_index.map(|index| &self.stylesheet_sources[index].source) == source.as_ref()
            && source_index.is_none_or(|index| self.stylesheet_sources[index].identity == identity) && linked_unchanged { return Ok(()); }
        if let Some(source) = &source {
            if let layout::StylesheetIdentity::Inline(text) = &identity {
                if source.text.as_ref() != text {
                    return Err(LayoutError::InvalidTree);
                }
            }
            css::parse_graph(source, self.media_environment()).map_err(LayoutError::Css)?;
        }
        if source.is_some() { self.stylesheet_sources.try_reserve(1).map_err(|_| LayoutError::InvalidTree)?; }
        let next_linked = match (&identity, &source) { (layout::StylesheetIdentity::Link(href), Some(source)) => Some((node, href.clone(), String::from(source.text.as_ref()))), _ => None };
        if next_linked.is_some() { self.linked_stylesheets.try_reserve(1).map_err(|_| LayoutError::InvalidTree)?; }
        let owner = css::StylesheetIdentity::Dom(node);
        let mut previous_overrides = Vec::new();
        previous_overrides.try_reserve(self.cssom_rule_overrides.iter().filter(|entry| entry.owner == owner).count()).map_err(|_| LayoutError::InvalidTree)?;
        let mut previous_topologies = Vec::new();
        previous_topologies.try_reserve(self.cssom_topology_overrides.iter().filter(|entry| entry.owner == owner).count()).map_err(|_| LayoutError::InvalidTree)?;
        // A host installation is a fresh source transaction. Old CSSOM state
        // must not validate or render the replacement, even for equal bytes.
        previous_overrides.extend(self.cssom_rule_overrides.extract_if(.., |entry| entry.owner == owner));
        previous_topologies.extend(self.cssom_topology_overrides.extract_if(.., |entry| entry.owner == owner));
        let overlay_index = self.cssom_stylesheet_texts.iter().position(|entry| entry.owner == node);
        let previous_overlay = overlay_index.map(|index| self.cssom_stylesheet_texts.remove(index));
        // Move only the affected owner's graph; rollback never clones siblings.
        let previous = source_index.map(|index| self.stylesheet_sources.remove(index));
        let linked_index = self.linked_stylesheets.iter().position(|(owner, _, _)| *owner == node);
        let previous_linked = linked_index.map(|index| self.linked_stylesheets.remove(index));
        if let Some(linked) = next_linked { self.linked_stylesheets.push(linked); }
        if let Some(source) = source { self.stylesheet_sources.push(layout::LoadedStylesheet { owner: node, identity, source, link_state: None, metadata: None }); }
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            self.stylesheet_sources.retain(|loaded| loaded.owner != node);
            self.linked_stylesheets.retain(|(owner, _, _)| *owner != node);
            if let (Some(index), Some(previous)) = (source_index, previous) { self.stylesheet_sources.insert(index, previous); }
            if let (Some(index), Some(previous)) = (linked_index, previous_linked) { self.linked_stylesheets.insert(index, previous); }
            self.cssom_rule_overrides.extend(previous_overrides);
            self.cssom_topology_overrides.extend(previous_topologies);
            if let (Some(index), Some(previous)) = (overlay_index, previous_overlay) { self.cssom_stylesheet_texts.insert(index, previous); }
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        let replacing = previous.is_some() || previous_linked.is_some() || previous_overlay.is_some();
        if replacing {
            let token = self.stylesheet_change_tokens.iter().find(|(owner, _)| *owner == node).and_then(|(_, token)| token.upgrade());
            let epoch = token.as_ref().map(|token| token.get());
            if let Some(lease) = self.stylesheet_graph_leases.iter().find(|(owner, seen, _)| *owner == node && Some(*seen) == epoch).and_then(|(_, _, lease)| lease.upgrade()) {
                lease.disabled.set(previous_disabled);
                lease.detached.set(true);
                if let Some(previous) = previous { *lease.source.borrow_mut() = Some(Box::new(previous.source)); }
            }
            if let Some(token) = token { token.set(token.get().wrapping_add(1)); }
        } else if let Some(source) = self.stylesheet_source(node) {
            for (owner, _, lease) in &self.stylesheet_graph_leases {
                if *owner == node { if let Some(lease) = lease.upgrade().filter(|lease| !lease.is_detached()) { *lease.text.borrow_mut() = source.text.clone(); } }
            }
        }
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Update applicability without replacing the sheet or its CSSOM source epoch.
    pub fn complete_inline_stylesheet_imports(&mut self,node:NodeId,source:css::StylesheetSource)->Result<(),LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        let index=self.stylesheet_sources.iter().position(|loaded|loaded.owner==node
            && matches!(loaded.identity,layout::StylesheetIdentity::Inline(_)))
            .ok_or(LayoutError::InvalidTree)?;
        if self.stylesheet_sources[index].source.text!=source.text
            || self.stylesheet_sources[index].source.url!=source.url {return Err(LayoutError::InvalidTree)}
        css::parse_graph(&source,self.media_environment()).map_err(LayoutError::Css)?;
        let previous=core::mem::replace(&mut self.stylesheet_sources[index].source,source);
        self.rules=None;
        if let Err(error)=self.refresh_rules() {
            self.stylesheet_sources[index].source=previous;self.rules=None;
            let _=self.refresh_rules();return Err(error)
        }
        self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(())
    }

    pub fn set_stylesheet_metadata(&mut self,node:NodeId,title:&str,media:&str)->Result<(),LayoutError> {
        let error=||LayoutError::Css(css::CssError{offset:0,message:"stylesheet metadata exceeds limit"});
        let bytes=layout::StylesheetMetadata::retained_bytes(title,media).ok_or_else(error)?;
        let index=self.stylesheet_sources.iter().position(|loaded|loaded.owner==node).ok_or(LayoutError::InvalidTree)?;
        let budget=self.stylesheet_metadata_budget.get_or_insert_with(||Rc::new(Cell::new(0))).clone();
        let next=budget.get().checked_add(bytes).filter(|bytes|*bytes<=css::MAX_CSS_BYTES).ok_or_else(error)?;
        let metadata=Rc::new(layout::StylesheetMetadata{title:Arc::from(title),media:RefCell::new(Arc::from(media)),budget:budget.clone()});
        budget.set(next);
        let previous=self.stylesheet_sources[index].metadata.replace(metadata);
        self.rules=None;
        if let Err(error)=self.refresh_rules() {self.stylesheet_sources[index].metadata=previous;self.rules=None;let _=self.refresh_rules();return Err(error);}
        self.invalidate_paint();Ok(())
    }
    pub fn set_stylesheet_media(&mut self,node:NodeId,metadata:&Rc<layout::StylesheetMetadata>,media:&str)->Result<(),LayoutError> {
        let previous_length=metadata.media.borrow().len();
        let next=metadata.budget.get().checked_sub(previous_length).and_then(|bytes|bytes.checked_add(media.len())).filter(|bytes|*bytes<=css::MAX_CSS_BYTES)
            .ok_or(LayoutError::Css(css::CssError{offset:0,message:"stylesheet metadata exceeds limit"}))?;
        let previous=metadata.media.replace(Arc::from(media));
        let previous_budget=metadata.budget.replace(next);
        let active=self.stylesheet_sources.iter().any(|loaded|loaded.owner==node && loaded.metadata.as_ref().is_some_and(|current|Rc::ptr_eq(current,metadata)));
        if active {self.rules=None;if let Err(error)=self.refresh_rules(){metadata.media.replace(previous);metadata.budget.set(previous_budget);self.rules=None;let _=self.refresh_rules();return Err(error);}self.invalidate_paint();}
        Ok(())
    }

    pub fn set_link_stylesheet_state(&mut self, node: NodeId, state: layout::LinkStylesheetState) -> Result<(), LayoutError> {
        let loaded = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node)
            .ok_or(LayoutError::InvalidTree)?;
        if !matches!(loaded.identity, layout::StylesheetIdentity::Link(_)) { return Err(LayoutError::InvalidTree); }
        for (owner,_,lease) in &self.stylesheet_graph_leases {if *owner==node {if let Some(lease)=lease.upgrade().filter(|lease|!lease.is_detached()) {lease.set_disabled(state.disabled);}}}
        if loaded.link_state == Some(state) { return Ok(()); }
        loaded.link_state = Some(state);
        self.rules = None;
        self.refresh_rules()?;
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    pub fn link_stylesheet_state(&self, node: NodeId) -> Option<layout::LinkStylesheetState> {
        self.stylesheet_source(node)?;
        self.stylesheet_sources.iter().find(|loaded| loaded.owner == node)?.link_state
    }

    pub fn stylesheet_disabled(&self,node:NodeId)->bool {
        let leased=self.stylesheet_graph_leases.iter().find_map(|(owner,_,lease)|(*owner==node).then(||lease.upgrade()).flatten().filter(|lease|!lease.is_detached()));
        if let Some(state)=self.link_stylesheet_state(node) {
            let attribute=self.document.get_attribute_ns_ref(node,None,"disabled").ok().flatten().is_some();
            return if state.attribute_disabled!=attribute {attribute}else{leased.as_ref().map_or(state.disabled,|lease|lease.disabled())};
        }
        leased.as_ref().map_or_else(||self.disabled_stylesheets.contains(&node),|lease|lease.disabled())
    }
    pub fn set_stylesheet_disabled(&mut self,node:NodeId,disabled:bool)->Result<(),LayoutError> {
        for (owner,_,lease) in &self.stylesheet_graph_leases {if *owner==node {if let Some(lease)=lease.upgrade().filter(|lease|!lease.is_detached()) {lease.set_disabled(disabled);}}}
        let projected=self.stylesheet_sources.iter().find(|source|source.owner==node).and_then(|source|source.link_state).map_or_else(||self.disabled_stylesheets.contains(&node),|state|state.disabled);
        if projected==disabled {return Ok(());}
        let mut linked=false;
        if let Some(loaded)=self.stylesheet_sources.iter_mut().find(|source|source.owner==node) {
            if let Some(state)=&mut loaded.link_state {state.disabled=disabled;linked=true;}
        }
        if disabled && !linked && !self.disabled_stylesheets.contains(&node) {
            self.disabled_stylesheets.try_reserve(1).map_err(|_|LayoutError::InvalidTree)?;
            self.disabled_stylesheets.push(node);
        }else if !disabled || linked {self.disabled_stylesheets.retain(|owner|*owner!=node);}
        self.rules=None;self.refresh_rules()?;self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(())
    }
    /// Publish a batch of pure owner/controller flag changes in one CSS refresh.
    pub fn flush_stylesheet_flag_changes(&mut self)->Result<(),LayoutError> {
        self.rules=None;self.refresh_rules()?;self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(())
    }
    pub fn set_preferred_stylesheet_set(&mut self,title:&str)->Result<(),LayoutError> {
        if self.preferred_stylesheet_set.as_deref()==Some(title) {return Ok(())}
        self.preferred_stylesheet_set=Some(Arc::from(title));
        self.rules=None;self.refresh_rules()?;self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(())
    }

    // A pending link refetch keeps its original associated sheet and CSSOM
    // identity until the resource-processing transaction replaces it.
    fn associated_stylesheet_identity(&self,node:NodeId)->Result<Option<layout::StylesheetIdentity>,LayoutError> {
        if let Some(loaded)=self.stylesheet_sources.iter().find(|source|source.owner==node && source.link_state.is_some()) {
            if self.document.kind(node).is_ok() {return Ok(Some(loaded.identity.clone()))}
        }
        layout::stylesheet_identity(&self.document,node)
    }

    pub fn stylesheet_source(&self, node: NodeId) -> Option<&css::StylesheetSource> {
        let loaded = self.stylesheet_sources.iter().find(|source| source.owner == node)?;
        if loaded.link_state.is_some() && self.document.kind(node).is_ok() && !crate::xml_stylesheet::is_candidate(&self.document,node) {return Some(&loaded.source)}
        if self.pending_external_stylesheet_mutation(node) { return None; }
        let identity = layout::stylesheet_identity(&self.document, node).ok()??;
        Some(loaded)
            .filter(|source| source.identity == identity)
            .filter(|source| match &source.identity {
                layout::StylesheetIdentity::Link(_) => {
                    self.linked_stylesheet(node) == Some(source.source.text.as_ref())
                }
                layout::StylesheetIdentity::Inline(_) => true,
            })
            .map(|source| &source.source)
    }

    pub fn imported_stylesheet_url(&self, node: NodeId, path: &[usize]) -> Option<&str> {
        Some(self.imported_stylesheet_at(node, path)?.url.as_ref())
    }

    pub fn imported_stylesheet_text(&self, node: NodeId, path: &[usize]) -> Option<String> {
        self.imported_stylesheet_text_ref(node, path).map(String::from)
    }

    pub fn imported_stylesheet_text_ref(&self, node: NodeId, path: &[usize]) -> Option<&str> {
        Some(self.imported_stylesheet_at(node, path)?.text.as_ref())
    }

    /// Move a retained removed import's graph without copying loaded children.
    /// The caller restores it at the same path if its source transaction fails.
    pub fn take_imported_stylesheet(&mut self, node: NodeId, path: &[usize]) -> Option<Box<css::StylesheetSource>> {
        let (&index, parent) = path.split_last()?;
        if path.len() > 32 { return None; }
        let loaded = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node)?;
        import_source_mut(&mut loaded.source, parent)?.imports.get_mut(index)?.source.take()
    }

    pub fn restore_imported_stylesheet(&mut self, node: NodeId, path: &[usize], source: Box<css::StylesheetSource>) -> Result<(), Box<css::StylesheetSource>> {
        let Some((&index, parent)) = path.split_last() else { return Err(source); };
        let target = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node)
            .and_then(|loaded| import_source_mut(&mut loaded.source, parent))
            .and_then(|parent| parent.imports.get_mut(index));
        match target { Some(import) if import.source.is_none() => { import.source = Some(source); self.rules = None; self.invalidate_paint(); Ok(()) }, _ => Err(source) }
    }

    fn imported_stylesheet_at(&self, node: NodeId, path: &[usize]) -> Option<&css::StylesheetSource> {
        let mut source = self.stylesheet_source(node)?;
        for &index in path {
            source = source.imports.get(index)?.source.as_deref()?;
        }
        Some(source)
    }

    /// A rare native CSSOM owner can retain this clock to detect external ABA
    /// edits even when the current serialized stylesheet bytes are unchanged.
    pub fn stylesheet_change_token(&mut self, node: NodeId) -> Result<Rc<Cell<u64>>, LayoutError> {
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { name, .. }) if matches!(crate::svg::local_name(name), "style" | "link")) && !crate::xml_stylesheet::is_candidate(&self.document,node) { return Err(LayoutError::InvalidTree); }
        self.reclaim_stale_stylesheet_sources();
        self.stylesheet_change_tokens.retain(|(owner, token)| token.strong_count() != 0 && self.document.kind(*owner).is_ok());
        if let Some(token) = self.stylesheet_change_tokens.iter().find(|(owner, _)| *owner == node).and_then(|(_, token)| token.upgrade()) { return Ok(token); }
        if self.stylesheet_change_tokens.len() >= 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM stylesheet owner limit exceeded" })); }
        self.stylesheet_change_tokens.try_reserve(1).map_err(|_| LayoutError::Css(css::CssError { offset: 0, message: "CSSOM stylesheet owner allocation failed" }))?;
        let token = Rc::new(Cell::new(0));
        self.stylesheet_change_tokens.push((node, Rc::downgrade(&token)));
        Ok(token)
    }

    pub fn stylesheet_root_lease(&mut self, owner: NodeId) -> Result<Rc<StylesheetGraphLease>, LayoutError> {
        let token = self.stylesheet_change_token(owner)?;
        let epoch = token.get();
        self.stylesheet_graph_leases.retain(|(_, _, lease)| lease.strong_count() != 0);
        let existing = self.stylesheet_graph_leases.iter().find(|(node, seen, _)| *node == owner && *seen == epoch).and_then(|(_, _, lease)| lease.upgrade());
        let graph = match existing {
            Some(graph) => graph,
            None => {
                if self.stylesheet_graph_leases.len() >= 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSS graph lease owner limit exceeded" })); }
                self.stylesheet_graph_leases.try_reserve(1).map_err(|_| LayoutError::Css(css::CssError { offset: 0, message: "CSS graph lease allocation failed" }))?;
                let text = if let Some(source) = self.stylesheet_source(owner) { source.text.clone() }
                    else if let Some(entry) = self.cssom_stylesheet_texts.iter().find(|entry| entry.owner == owner) { entry.text.clone() }
                    else { match layout::stylesheet_identity(&self.document, owner)? {
                        Some(layout::StylesheetIdentity::Inline(text)) => Arc::from(text),
                        Some(layout::StylesheetIdentity::Link(_)) => Arc::from(self.linked_stylesheet(owner).unwrap_or_default()),
                        None => return Err(LayoutError::InvalidTree),
                    } };
                if text.len() > 1024 * 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM source exceeds limit" })); }
                let graph = Rc::new(StylesheetGraphLease { _epoch_token: token, text: RefCell::new(text), detached: Cell::new(false), disabled: Cell::new(self.stylesheet_disabled(owner)), source: RefCell::new(None), imports: RefCell::new(Vec::new()), metadata: self.stylesheet_sources.iter().find(|loaded|loaded.owner==owner).and_then(|loaded|loaded.metadata.clone()) });
                self.stylesheet_graph_leases.push((owner, epoch, Rc::downgrade(&graph)));
                graph
            }
        };
        Ok(graph)
    }

    pub fn stylesheet_import_lease(&mut self, owner: NodeId, path: Vec<usize>) -> Result<Rc<StylesheetImportLease>, LayoutError> {
        if self.imported_stylesheet_at(owner,&path).is_none() {return Err(LayoutError::InvalidTree)}
        self.stylesheet_import_rule_lease(owner,path)
    }
    /// Pending rules need the same occurrence identity before a child exists.
    pub fn stylesheet_import_rule_lease(&mut self,owner:NodeId,path:Vec<usize>)->Result<Rc<StylesheetImportLease>,LayoutError> {
        if path.is_empty() || path.len()>32 || self.stylesheet_source(owner).and_then(|source|import_rule_ref(source,&path)).is_none() {return Err(LayoutError::InvalidTree)}
        let graph=self.stylesheet_root_lease(owner)?;
        let mut imports=graph.imports.borrow_mut();imports.retain(|lease|lease.strong_count()!=0);
        if let Some(existing)=imports.iter().filter_map(Weak::upgrade).find(|lease|lease.path.borrow().as_deref()==Some(path.as_slice())) {return Ok(existing)}
        if imports.len()>=css::MAX_CSS_GRAPH_IMPORTS {return Err(LayoutError::Css(css::CssError{offset:0,message:"CSS graph lease import limit exceeded"}))}
        imports.try_reserve(1).map_err(|_|LayoutError::Css(css::CssError{offset:0,message:"CSS import lease allocation failed"}))?;
        let lease=Rc::new(StylesheetImportLease{graph:graph.clone(),path:RefCell::new(Some(path))});
        imports.push(Rc::downgrade(&lease));Ok(lease)
    }
    pub fn stylesheet_import_rule<'a>(&'a self,owner:NodeId,lease:&StylesheetImportLease)->Option<&'a css::LoadedImport> {
        if !self.stylesheet_graph_leases.iter().any(|(node,epoch,graph)|*node==owner && *epoch==lease.graph._epoch_token.get()
            && graph.upgrade().is_some_and(|graph|Rc::ptr_eq(&graph,&lease.graph))) {return None}
        lease.with_live_path(|path|path.and_then(|path|self.stylesheet_source(owner).and_then(|source|import_rule_ref(source,path))))
    }
    /// Complete only live occurrences, preserving root bytes, CSSOM overrides,
    /// untouched subgraphs and every existing rule/stylesheet wrapper identity.
    pub fn complete_stylesheet_import_occurrences(&mut self,owner:NodeId,completed:Vec<(Rc<StylesheetImportLease>,Box<css::StylesheetSource>)>)->Result<usize,LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        if completed.len()>css::MAX_CSS_GRAPH_IMPORTS {return Err(LayoutError::InvalidTree)}
        let mut prepared:Vec<(Rc<StylesheetImportLease>,Vec<usize>,Box<css::StylesheetSource>)>=Vec::new();prepared.try_reserve(completed.len()).map_err(|_|LayoutError::InvalidTree)?;
        for (lease,source) in completed {
            if self.stylesheet_import_rule(owner,&lease).is_none() {continue}
            if prepared.iter().any(|(known,_,_)|Rc::ptr_eq(known,&lease)) {return Err(LayoutError::InvalidTree)}
            let path=lease.with_live_path(|path| {
                let Some(path)=path else{return Err(LayoutError::InvalidTree)};
                let mut captured=Vec::new();captured.try_reserve_exact(path.len()).map_err(|_|LayoutError::InvalidTree)?;
                captured.extend_from_slice(path);Ok(captured)
            })?;
            if prepared.iter().any(|(_,known,_)|path.starts_with(known)||known.starts_with(&path)) {return Err(LayoutError::InvalidTree)}
            prepared.push((lease,path,source));
        }
        let Some(index)=self.stylesheet_sources.iter().position(|entry|entry.owner==owner) else{return Ok(0)};
        let mut changes=Vec::new();changes.try_reserve(prepared.len()).map_err(|_|LayoutError::InvalidTree)?;
        for (_,path,source) in prepared {
            let Some(rule)=import_rule_mut(&mut self.stylesheet_sources[index].source,&path) else {continue};
            let previous=core::mem::replace(&mut rule.source,Some(source));changes.push((path,previous));
        }
        if changes.is_empty() {return Ok(0)}
        let index=self.stylesheet_sources.iter().position(|entry|entry.owner==owner).ok_or(LayoutError::InvalidTree)?;
        let valid=css::parse_graph_with_topology(&self.stylesheet_sources[index].source,self.media_environment(),css::StylesheetIdentity::Dom(owner),&self.cssom_topology_overrides).map_err(LayoutError::Css);
        self.rules=None;
        let result=valid.and_then(|_|self.refresh_rules());
        if let Err(error)=result {
            for (path,previous) in changes.into_iter().rev() {if let Some(rule)=import_rule_mut(&mut self.stylesheet_sources[index].source,&path) {rule.source=previous;}}
            self.rules=None;let _=self.refresh_rules();return Err(error)
        }
        let count=changes.len();self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(count)
    }

    fn rebase_stylesheet_import_leases(&mut self, owner: NodeId, path: &[usize], mapping: Option<&[Option<usize>]>) {
        let Some(mapping) = mapping else { return; };
        for (node, _, graph) in &self.stylesheet_graph_leases {
            if *node != owner { continue; }
            if let Some(graph) = graph.upgrade().filter(|graph| graph.source.borrow().is_none()) { graph.rebase_imports(path, mapping); }
        }
    }

    fn pending_external_stylesheet_mutation(&self, owner: NodeId) -> bool {
        let version = self.document.version();
        if version == self.stylesheet_mutation_version { return false; }
        let protected = self.stylesheet_owned_write.is_some_and(|(node, _)| node == owner);
        let mut observed = false;
        for mutation in self.document.mutations().iter().filter(|mutation| mutation.version > self.stylesheet_mutation_version) {
            observed = true;
            if self.stylesheet_owned_write.is_some_and(|(node, start)| node == owner && mutation.version > start) { continue; }
            if matches!(mutation.kind, MutationKind::FullRebuild) || stylesheet_source_mutation_owner(&self.document, mutation) == Some(owner)
                || (crate::xml_stylesheet::is_candidate(&self.document,owner) && matches!(mutation.kind,MutationKind::Tree{added,removed,..} if added==Some(owner) || removed==Some(owner))) { return true; }
        }
        !observed && !protected
    }

    /// Reclaim native source snapshots and CSSOM renderer overlays after raw
    /// DOM source edits, including changes away from and back to equal bytes.
    /// Mutation generations, rather than content equality, prevent revival.
    pub fn reclaim_stale_stylesheet_sources(&mut self) {
        let version = self.document.version();
        if version == self.stylesheet_mutation_version { return; }
        let mut owners = Vec::new();
        for (owner,seen,lease) in &self.stylesheet_graph_leases {
            if lease.upgrade().is_some_and(|lease|!lease.is_detached())
                && self.stylesheet_change_tokens.iter().find(|(node,_)|node==owner)
                    .and_then(|(_,token)|token.upgrade()).is_some_and(|token|token.get()!=*seen)
                && !owners.contains(owner) {owners.push(*owner);}
        }
        for loaded in &self.stylesheet_sources {
            if crate::xml_stylesheet::is_candidate(&self.document,loaded.owner)
                && !crate::xml_stylesheet::in_prolog(&self.document,loaded.owner).unwrap_or(false)
                && !owners.contains(&loaded.owner) {owners.push(loaded.owner);}
        }
        let mut observed = false;
        let mut all = false;
        for mutation in self.document.mutations().iter().filter(|mutation| mutation.version > self.stylesheet_mutation_version) {
            observed = true;
            if matches!(mutation.kind, MutationKind::FullRebuild) { all = true; continue; }
            if let MutationKind::Tree{added,removed,..}=&mutation.kind {
                for owner in added.iter().chain(removed.iter()).copied().filter(|owner|crate::xml_stylesheet::is_candidate(&self.document,*owner)) {
                    if !owners.contains(&owner) {owners.push(owner);}
                }
                if let Some(removed) = removed {
                    // Removing a sheet owner (or its shadow-including ancestor)
                    // destroys the association even if it is reinserted before
                    // this safe retirement boundary. Keep the old graph on its
                    // existing lease rather than reviving an equal source.
                    for owner in self.stylesheet_sources.iter().map(|loaded|loaded.owner)
                        .chain(self.stylesheet_graph_leases.iter().map(|(owner,_,_)|*owner)) {
                        if self.document.is_host_including_inclusive_ancestor(*removed,owner).unwrap_or(false)
                            && !owners.contains(&owner) {owners.push(owner);}
                    }
                }
            }
            if let Some(owner) = stylesheet_source_mutation_owner(&self.document, mutation) {
                if matches!(&mutation.kind,MutationKind::Attribute(name) if name.eq_ignore_ascii_case("href"))
                    && self.stylesheet_sources.iter().any(|loaded|loaded.owner==owner && loaded.link_state.is_some()) {continue}
                if self.stylesheet_owned_write.is_some_and(|(node, start)| node == owner && mutation.version > start) { continue; }
                if !owners.contains(&owner) { owners.push(owner); }
            }
        }
        all |= !observed;
        let protected = self.stylesheet_owned_write.map(|(owner, _)| owner);
        let document = &self.document;
        let affected = |owner: NodeId| document.kind(owner).is_err() || owners.contains(&owner) || (all && protected != Some(owner));
        self.stylesheet_graph_leases.retain(|(_, _, lease)| lease.strong_count() != 0);
        for loaded in self.stylesheet_sources.extract_if(.., |loaded| affected(loaded.owner)) {
            let owner = loaded.owner;
            let lease = self.stylesheet_graph_leases.iter().find_map(|(node,_,lease)|
                (*node==owner).then(||lease.upgrade()).flatten().filter(|lease|!lease.is_detached()));
            if let Some(lease) = lease {
                *lease.source.borrow_mut() = Some(alloc::boxed::Box::new(loaded.source));
            }
        }
        for (owner, _, lease) in &self.stylesheet_graph_leases {
            if affected(*owner) { if let Some(lease) = lease.upgrade() {
                lease.detached.set(true);
            } }
        }
        self.disabled_stylesheets.retain(|owner|!affected(*owner));
        self.cssom_stylesheet_texts.retain(|entry| !affected(entry.owner));
        self.stylesheet_change_tokens.retain(|(owner, token)| {
            let Some(token) = token.upgrade() else { return false; };
            if self.document.kind(*owner).is_err() { return false; }
            if affected(*owner) { token.set(token.get().wrapping_add(1)); }
            true
        });
        self.linked_stylesheets.retain(|(owner, _, _)| !affected(*owner));
        self.cssom_rule_overrides.retain(|entry| !matches!(entry.owner, css::StylesheetIdentity::Dom(owner) if affected(owner)));
        self.cssom_topology_overrides.retain(|entry| !matches!(entry.owner, css::StylesheetIdentity::Dom(owner) if affected(owner)));
        self.stylesheet_mutation_version = version;
    }

    /// Replace an inline stylesheet's text and captured loaded graph together.
    /// Existing direct text-node identity is preserved when possible.
    pub fn replace_stylesheet_text(&mut self, node: NodeId, text: &str) -> Result<(), LayoutError> {
        self.replace_stylesheet_text_with_import_map(node, text, None)
    }

    /// Commit CSSOM bytes without mutating author nodes or DOM mutation records.
    /// A detached native sheet edits only its lease, independent of the owner.
    pub fn replace_cssom_stylesheet_text(&mut self, node: NodeId, lease: &Rc<StylesheetGraphLease>, text: &str, imports: Option<&[Option<usize>]>) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        if text.len() > 1024 * 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM source exceeds limit" })); }
        let text: Arc<str> = Arc::from(text);
        if lease.is_detached() {
            let mut graph = lease.source.borrow_mut();
            if let Some(graph) = graph.as_deref_mut() {
                let edit = css::prepare_stylesheet_text_edit_with_import_map(graph, text.clone(), imports).map_err(LayoutError::Css)?;
                if let Err(error) = css::parse_graph(graph, self.media_environment()) { edit.rollback(graph); return Err(LayoutError::Css(error)); }
            } else { css::parse_stylesheet(&text).map_err(LayoutError::Css)?; }
            *lease.text.borrow_mut() = text;
            if let Some(mapping) = imports { lease.rebase_imports(&[], mapping); }
            return Ok(());
        }
        if !self.stylesheet_graph_leases.iter().any(|(owner, epoch, candidate)| *owner == node && *epoch == lease._epoch_token.get() && candidate.upgrade().is_some_and(|candidate| Rc::ptr_eq(&candidate, lease))) {
            return Err(LayoutError::InvalidTree);
        }
        let identity = self.associated_stylesheet_identity(node)?.ok_or(LayoutError::InvalidTree)?;
        let index = self.cssom_stylesheet_texts.iter().position(|entry| entry.owner == node);
        let bytes = self.cssom_stylesheet_texts.iter().filter(|entry| entry.owner != node).map(|entry| entry.text.len()).sum::<usize>();
        if bytes.saturating_add(text.len()) > 8 * 1024 * 1024 || (index.is_none() && self.cssom_stylesheet_texts.len() >= 1024) {
            return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM effective source limit exceeded" }));
        }
        if index.is_none() { self.cssom_stylesheet_texts.try_reserve(1).map_err(|_| LayoutError::Css(css::CssError { offset: 0, message: "CSSOM source allocation failed" }))?; }
        let source_index = self.stylesheet_sources.iter().position(|entry| entry.owner == node && entry.identity == identity);
        let edit = if let Some(index) = source_index {
            let edit = css::prepare_stylesheet_text_edit_with_import_map(&mut self.stylesheet_sources[index].source, text.clone(), imports).map_err(LayoutError::Css)?;
            if let Err(error) = css::parse_graph_with_topology(&self.stylesheet_sources[index].source, self.media_environment(), css::StylesheetIdentity::Dom(node), &self.cssom_topology_overrides) {
                edit.rollback(&mut self.stylesheet_sources[index].source);
                return Err(LayoutError::Css(error));
            }
            Some(edit)
        } else { css::parse_stylesheet(&text).map_err(LayoutError::Css)?; None };
        let next = layout::CssomStylesheetText { owner: node, identity, text: text.clone() };
        let previous = if let Some(index) = index { Some(core::mem::replace(&mut self.cssom_stylesheet_texts[index], next)) }
            else { self.cssom_stylesheet_texts.push(next); None };
        // Linked snapshots compare their bytes against the graph, while author
        // href identity remains unchanged. Inline snapshots need no DOM writes.
        let linked_index = self.linked_stylesheets.iter().position(|(owner, _, _)| *owner == node);
        let previous_linked = linked_index.map(|index| core::mem::replace(&mut self.linked_stylesheets[index].2, String::from(text.as_ref())));
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            if let Some(index) = index { self.cssom_stylesheet_texts[index] = previous.unwrap(); } else { self.cssom_stylesheet_texts.pop(); }
            if let (Some(index), Some(previous)) = (linked_index, previous_linked) { self.linked_stylesheets[index].2 = previous; }
            if let (Some(index), Some(edit)) = (source_index, edit) { edit.rollback(&mut self.stylesheet_sources[index].source); }
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        *lease.text.borrow_mut() = text;
        self.rebase_stylesheet_import_leases(node, &[], imports);
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    pub fn replace_stylesheet_text_with_import_map(&mut self, node: NodeId, text: &str, imports: Option<&[Option<usize>]>) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        let identity = self.associated_stylesheet_identity(node)?.ok_or(LayoutError::InvalidTree)?;
        if let layout::StylesheetIdentity::Link(_) = &identity {
            return self.replace_linked_stylesheet_text(node, text, imports);
        }
        let mut children = Vec::new();
        let mut child = self.document.first_child(node).map_err(|_| LayoutError::InvalidTree)?;
        while let Some(id) = child {
            children.push(id);
            child = self.document.next_sibling(id).map_err(|_| LayoutError::InvalidTree)?;
        }
        let existing = children.iter().copied().find(|id| matches!(self.document.kind(*id), Ok(NodeKind::Text(_))));
        let previous_data = existing.and_then(|id| match self.document.kind(id).ok()? {
            NodeKind::Text(text) => Some(text.clone()), _ => None,
        });
        let environment = self.media_environment();
        let loaded_index = self.stylesheet_sources.iter().position(|loaded| loaded.owner == node && loaded.identity == identity);
        let edit = if let Some(index) = loaded_index {
            let edit = css::prepare_stylesheet_text_edit_with_import_map(&mut self.stylesheet_sources[index].source, Arc::from(text), imports).map_err(LayoutError::Css)?;
            if let Err(error) = css::parse_graph_with_topology(&self.stylesheet_sources[index].source, environment, css::StylesheetIdentity::Dom(node), &self.cssom_topology_overrides) {
                edit.rollback(&mut self.stylesheet_sources[index].source);
                return Err(LayoutError::Css(error));
            }
            Some(edit)
        } else {
            let boundaries = self.cssom_topology_overrides.iter().rev().find(|entry|
                entry.owner == css::StylesheetIdentity::Dom(node) && entry.source_url.is_none() && entry.source_text.as_ref() == text
            ).map_or(&[][..], |entry| entry.boundaries.as_ref());
            css::parse_stylesheet_with_boundaries(text, boundaries).map_err(LayoutError::Css)?;
            None
        };
        let previous_owned_write = self.stylesheet_owned_write;
        self.stylesheet_owned_write = Some((node, self.document.version()));
        let mut created = None;
        let mutation: Result<NodeId, LayoutError> = (|| {
            let text_node = if let Some(existing) = existing { existing } else {
                let id = self.document.create(NodeKind::Text(String::new())).map_err(|_| LayoutError::InvalidTree)?;
                created = Some(id);
                id
            };
            self.document.replace_data(text_node, text).map_err(|_| LayoutError::InvalidTree)?;
            self.document.replace_children_many(node, &[text_node]).map_err(|_| LayoutError::InvalidTree)?;
            if let Some(index) = loaded_index {
                self.stylesheet_sources[index].identity = layout::StylesheetIdentity::Inline(text.into());
            }
            self.rules = None;
            self.refresh_rules()?;
            Ok(text_node)
        })();
        let text_node = match mutation {
            Ok(node) => node,
            Err(error) => {
                if let Some(existing) = existing {
                    if let Some(previous_data) = previous_data { let _ = self.document.replace_data(existing, &previous_data); }
                }
                let _ = self.document.replace_children_many(node, &children);
                if let Some(created) = created { let _ = self.document.destroy_subtree(created); }
                if let Some(edit) = edit {
                    if let Some(loaded) = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node) {
                        edit.rollback(&mut loaded.source);
                        loaded.identity = identity;
                    }
                }
                self.rules = None;
                let _ = self.refresh_rules();
                self.reclaim_stale_stylesheet_sources();
                self.stylesheet_owned_write = previous_owned_write;
                return Err(error);
            }
        };
        for removed in children {
            if removed != text_node {
                if self.document.destroy_subtree(removed).is_err() {
                    self.reclaim_stale_stylesheet_sources();
                    self.stylesheet_owned_write = previous_owned_write;
                    return Err(LayoutError::InvalidTree);
                }
            }
        }
        self.reclaim_stale_stylesheet_sources();
        self.stylesheet_owned_write = previous_owned_write;
        self.rebase_stylesheet_import_leases(node, &[], imports);
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    fn replace_linked_stylesheet_text(&mut self, node: NodeId, text: &str, imports: Option<&[Option<usize>]>) -> Result<(), LayoutError> {
        let identity = self.associated_stylesheet_identity(node)?.ok_or(LayoutError::InvalidTree)?;
        let Some(source_index) = self.stylesheet_sources.iter().position(|loaded| loaded.owner == node && loaded.identity == identity) else {
            return self.set_linked_stylesheet(node, Some(text.into()));
        };
        let linked_index = self.linked_stylesheets.iter().position(|(owner, _, _)| *owner == node).ok_or(LayoutError::InvalidTree)?;
        let edit = css::prepare_stylesheet_text_edit_with_import_map(&mut self.stylesheet_sources[source_index].source, Arc::from(text), imports).map_err(LayoutError::Css)?;
        if let Err(error) = css::parse_graph_with_topology(&self.stylesheet_sources[source_index].source, self.media_environment(), css::StylesheetIdentity::Dom(node), &self.cssom_topology_overrides) {
            edit.rollback(&mut self.stylesheet_sources[source_index].source);
            return Err(LayoutError::Css(error));
        }
        let previous = core::mem::replace(&mut self.linked_stylesheets[linked_index].2, text.into());
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            if let Some((_, _, linked)) = self.linked_stylesheets.iter_mut().find(|(owner, _, _)| *owner == node) { *linked = previous; }
            if let Some(loaded) = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node) { edit.rollback(&mut loaded.source); }
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.rebase_stylesheet_import_leases(node, &[], imports);
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Change an actual imported sheet's flag without editing its import rule
    /// or replacing its graph. The lease follows structural occurrence moves.
    pub fn set_stylesheet_import_disabled(&mut self, owner: NodeId, lease: &StylesheetImportLease, disabled: bool) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        if !self.stylesheet_graph_leases.iter().any(|(node,epoch,graph)|*node==owner && *epoch==lease.graph._epoch_token.get()
            && graph.upgrade().is_some_and(|graph|Rc::ptr_eq(&graph,&lease.graph))) {return Err(LayoutError::InvalidTree);}
        let unchanged=lease.with_live_path(|path|path.and_then(|path|self.stylesheet_sources.iter().find(|loaded|loaded.owner==owner)
            .and_then(|loaded|import_source_ref(&loaded.source,path))).is_some_and(|source|source.disabled==disabled));
        if unchanged {return Ok(());}
        let path=lease.with_live_path(|path|path.map(|path| {
            let mut captured=Vec::new();captured.try_reserve_exact(path.len()).map_err(|_|LayoutError::InvalidTree)?;
            captured.extend_from_slice(path);Ok(captured)
        })).ok_or(LayoutError::InvalidTree)??;
        let index=self.stylesheet_sources.iter().position(|loaded|loaded.owner==owner).ok_or(LayoutError::InvalidTree)?;
        let source=import_source_mut(&mut self.stylesheet_sources[index].source,&path).ok_or(LayoutError::InvalidTree)?;
        let previous=source.disabled;
        if previous==disabled {return Ok(());}
        source.disabled=disabled;
        self.rules=None;
        if let Err(error)=self.refresh_rules() {
            if let Some(source)=import_source_mut(&mut self.stylesheet_sources[index].source,&path) {source.disabled=previous;}
            self.rules=None;let _=self.refresh_rules();return Err(error);
        }
        self.invalidate_paint();self.frame_id=self.frame_id.wrapping_add(1);Ok(())
    }

    pub fn replace_imported_stylesheet_text(&mut self, node: NodeId, path: &[usize], text: &str) -> Result<(), LayoutError> {
        self.replace_imported_stylesheet_text_with_import_map(node, path, text, None)
    }

    pub fn replace_imported_stylesheet_text_with_import_map(&mut self, node: NodeId, path: &[usize], text: &str, imports: Option<&[Option<usize>]>) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        if path.len() > 32 { return Err(LayoutError::InvalidTree); }
        let source_index = self.stylesheet_sources.iter().position(|loaded| loaded.owner == node).ok_or(LayoutError::InvalidTree)?;
        let child = import_source_mut(&mut self.stylesheet_sources[source_index].source, path).ok_or(LayoutError::InvalidTree)?;
        let edit = css::prepare_stylesheet_text_edit_with_import_map(child, Arc::from(text), imports).map_err(LayoutError::Css)?;
        if let Err(error) = css::parse_graph_with_topology(&self.stylesheet_sources[source_index].source, self.media_environment(), css::StylesheetIdentity::Dom(node), &self.cssom_topology_overrides) {
            edit.rollback(import_source_mut(&mut self.stylesheet_sources[source_index].source, path).unwrap());
            return Err(LayoutError::Css(error));
        }
        self.rules = None;
        if let Err(error) = self.refresh_rules() {
            if let Some(loaded) = self.stylesheet_sources.iter_mut().find(|loaded| loaded.owner == node) {
                if let Some(child) = import_source_mut(&mut loaded.source, path) { edit.rollback(child); }
            }
            self.rules = None;
            let _ = self.refresh_rules();
            return Err(error);
        }
        self.rebase_stylesheet_import_leases(node, path, imports);
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

    /// Host preference changes use the same canonical environment/cache key as
    /// viewport and media changes. Page support comes from the actual document.
    pub fn set_embedding_color_scheme(&mut self,scheme:css::UsedColorScheme)->Result<(),LayoutError> {
        if self.embedding_color_scheme!=Some(scheme) {self.embedding_color_scheme=Some(scheme);self.invalidate_paint();}
        self.set_color_scheme_preference(match scheme{css::UsedColorScheme::Light=>css::ColorSchemePreference::Light,css::UsedColorScheme::Dark=>css::ColorSchemePreference::Dark})
    }
    pub fn set_color_scheme_preference(&mut self,preference:css::ColorSchemePreference)->Result<(),LayoutError> {
        self.refresh_rules()?;
        let mut environment=self.media_environment();
        environment.color_schemes.preference=preference;
        self.set_media_environment(environment)
    }

    pub fn set_media_environment(
        &mut self,
        mut environment: css::MediaEnvironment,
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
        environment.color_schemes.page_support=self.rules.as_ref().expect("stylesheets initialized").environment.color_schemes.page_support;
        let rules = self.rules.as_mut().expect("stylesheets initialized");
        if rules.environment == environment && self.style_environment == Some(environment) {
            return Ok(());
        }
        rules.environment = environment;
        self.layout_cache.clear();
        // Cached styles were resolved in the previous environment. Publish the
        // new identity only after discarding those computed values.
        self.style_cache.borrow_mut().clear();
        self.style_environment = Some(environment);
        self.invalidate_paint();
        self.frame_id = self.frame_id.wrapping_add(1);
        Ok(())
    }

    /// Return the resource text belonging to a link's current href. Resource
    /// loading is a host responsibility; changing href hides the stale sheet.
    pub fn linked_stylesheet(&self, node: NodeId) -> Option<&str> {
        if self.stylesheet_sources.iter().any(|source|source.owner==node && source.link_state.is_some()) {
            return self.linked_stylesheets.iter().find(|(owner,_,_)|*owner==node).map(|(_,_,text)|text.as_str());
        }
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

    pub fn cssom_rule_overrides(&self) -> &[css::RuleDeclarationOverride] { &self.cssom_rule_overrides }
    pub fn cssom_topology_overrides(&self) -> &[css::RuleTopologyOverride] { &self.cssom_topology_overrides }

    pub fn cssom_rule_override(&self, owner: css::StylesheetIdentity, source_text: &str, source_url: Option<&str>, declaration_offset: usize) -> Option<&css::DeclarationBlock> {
        self.cssom_rule_overrides.iter().rev().find(|retained| retained.owner == owner && retained.source_text.as_ref() == source_text && retained.source_url.as_deref() == source_url && retained.declaration_offset == declaration_offset)
            .map(|retained| retained.block.as_ref())
    }

    /// Commit typed CSSOM rule state separately from its reflected CSS text.
    /// Exact owner/source/offset identity prevents stale blocks from applying
    /// after a direct stylesheet replacement or to another sheet with equal CSS.
    pub fn stage_cssom_rule_overrides(&mut self, overrides: Vec<css::RuleDeclarationOverride>) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        self.stage_cssom_state(overrides, self.cssom_topology_overrides.clone())
    }

    pub fn stage_cssom_state(&mut self, overrides: Vec<css::RuleDeclarationOverride>, topologies: Vec<css::RuleTopologyOverride>) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        if overrides.len() > 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM rule override limit exceeded" })); }
        let mut retained_bytes = 0usize;
        for (position, retained) in overrides.iter().enumerate() {
            let source_bytes = if overrides[..position].iter().any(|previous| alloc::sync::Arc::ptr_eq(&previous.source_text, &retained.source_text)) { 0 } else { retained.source_text.len() };
            retained_bytes = retained_bytes.checked_add(source_bytes).and_then(|bytes| bytes.checked_add(retained.block.retained_text_bytes()).and_then(|bytes| bytes.checked_add(retained.source_url.as_ref().map_or(0, |url| url.len()))).and_then(|bytes| retained.cssom_path.len().checked_mul(core::mem::size_of::<usize>()).and_then(|metadata| bytes.checked_add(metadata)))).ok_or(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM retained source budget exceeded" }))?;
            if retained_bytes > 8 * 1024 * 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM retained source budget exceeded" })); }
            if retained.source_text.len() > css::MAX_CSS_BYTES || retained.cssom_path.len() > 32 || retained.source_url.as_ref().is_some_and(|url| url.len() > 8192) || retained.declaration_offset >= retained.source_text.len() || !retained.source_text.is_char_boundary(retained.declaration_offset) {
                return Err(LayoutError::Css(css::CssError { offset: 0, message: "invalid CSSOM source identity" }));
            }
            retained.block.render_declarations().map_err(LayoutError::Css)?;
        }
        if topologies.len() > 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM topology owner limit exceeded" })); }
        for (position, topology) in topologies.iter().enumerate() {
            let invalid = topology.source_text.len() > css::MAX_CSS_BYTES || topology.boundaries.len() > css::nesting::MAX_DECLARATION_BOUNDARIES || topology.source_url.as_ref().is_some_and(|url| url.len() > 8192)
                || topology.boundaries.iter().any(|boundary| boundary.parent_start > boundary.range.start || boundary.child_index >= 4096 || boundary.range.start > boundary.range.end || boundary.range.end > topology.source_text.len() || !topology.source_text.is_char_boundary(boundary.range.start) || !topology.source_text.is_char_boundary(boundary.range.end));
            if invalid { return Err(LayoutError::Css(css::CssError { offset: 0, message: "invalid CSSOM declaration boundaries" })); }
            let source_bytes = if overrides.iter().any(|entry| alloc::sync::Arc::ptr_eq(&entry.source_text, &topology.source_text)) || topologies[..position].iter().any(|entry| alloc::sync::Arc::ptr_eq(&entry.source_text, &topology.source_text)) { 0 } else { topology.source_text.len() };
            let boundary_bytes = if topologies[..position].iter().any(|entry| alloc::sync::Arc::ptr_eq(&entry.boundaries, &topology.boundaries)) { 0 } else { topology.boundaries.len() * core::mem::size_of::<css::nesting::DeclarationBoundary>() };
            retained_bytes = retained_bytes.checked_add(source_bytes).and_then(|bytes| bytes.checked_add(boundary_bytes)).and_then(|bytes| bytes.checked_add(topology.source_url.as_ref().map_or(0, |url| url.len()))).ok_or(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM retained source budget exceeded" }))?;
            if retained_bytes > 8 * 1024 * 1024 { return Err(LayoutError::Css(css::CssError { offset: 0, message: "CSSOM retained source budget exceeded" })); }
        }
        self.cssom_rule_overrides = overrides;
        self.cssom_topology_overrides = topologies;
        self.rules = None;
        self.style_cache.borrow_mut().clear();
        self.read_style_cache.clear();
        self.read_style_key = None;
        self.animation_snapshot = None;
        self.invalidate_paint();
        self.frame_id += 1;
        Ok(())
    }

    pub fn set_cssom_rule_overrides(&mut self, overrides: Vec<css::RuleDeclarationOverride>) -> Result<(), LayoutError> {
        let mut previous = Vec::new();
        previous.try_reserve_exact(self.cssom_rule_overrides.len()).map_err(|_| LayoutError::Css(css::CssError { offset: 0, message: "CSSOM override allocation failed" }))?;
        previous.extend(self.cssom_rule_overrides.iter().cloned());
        self.stage_cssom_rule_overrides(overrides)?;
        if let Err(error) = self.refresh_rules() {
            self.stage_cssom_rule_overrides(previous)?;
            let _ = self.refresh_rules();
            return Err(error);
        }
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
        self.set_adopted_stylesheet_sources(scope,stylesheets.into_iter().map(Into::into).collect())
    }
    pub fn set_adopted_stylesheet_sources(&mut self,scope:Option<NodeId>,stylesheets:Vec<AdoptedStylesheetSource>)->Result<(),LayoutError> {
        for stylesheet in &stylesheets {
            css::parse_scoped(&stylesheet.text, scope).map_err(LayoutError::Css)?;
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

    /// Publish all constructible sheet scopes in one style-change transaction.
    pub fn replace_adopted_stylesheet_sources(&mut self,sources:Vec<(Option<NodeId>,Vec<AdoptedStylesheetSource>)>)->Result<(),LayoutError> {
        if sources==self.adopted_stylesheets {return Ok(());}
        for (scope,sheets) in &sources {for sheet in sheets {css::parse_scoped(&sheet.text,*scope).map_err(LayoutError::Css)?;}}
        let previous=core::mem::replace(&mut self.adopted_stylesheets,sources);
        self.rules=None;
        if let Err(error)=self.refresh_rules() {self.adopted_stylesheets=previous;self.rules=None;let _=self.refresh_rules();return Err(error);}
        self.invalidate_paint();self.frame_id+=1;Ok(())
    }

    /// Update one node's Web Animations declarations without mutating its DOM
    /// attributes or producing mutation-observer records.
    pub fn set_animation_declarations(&mut self, node: NodeId,
        declarations: Vec<(String,String)>) -> Result<(),LayoutError> {
        self.set_pseudo_effect_declarations(node,None,css::EffectOrigin::Animation,declarations)
    }

    pub fn set_effect_declarations(&mut self,node:NodeId,origin:css::EffectOrigin,
        declarations:Vec<(String,String)>) -> Result<(),LayoutError> {
        self.set_pseudo_effect_declarations(node,None,origin,declarations)
    }

    /// Publish one actual effect target at its declaration origin. Element and
    /// generated pseudo effects use the same bounded sparse publication store.
    pub fn set_pseudo_effect_declarations(&mut self,node:NodeId,pseudo:Option<css::PseudoElement>,
        origin:css::EffectOrigin,declarations:Vec<(String,String)>) -> Result<(),LayoutError> {
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules_only()?;
        let position = self
            .animated_styles
            .iter()
            .position(|(target, target_pseudo, target_origin, _)| *target == node && *target_pseudo == pseudo && *target_origin == origin);
        let paint_only = {
            let previous: &[(String, String)] =
                position.map_or(&[][..], |index| self.animated_styles[index].3.as_slice());
            if previous == declarations.as_slice() {
                return Ok(());
            }
            changed_declarations_paint_only(previous, &declarations)
        };
        match position {
            Some(index) if declarations.is_empty() => { self.animated_styles.remove(index); },
            Some(index) => self.animated_styles[index].3 = declarations.clone(),
            None if declarations.is_empty() => {},
            None => self.animated_styles.push((node, pseudo, origin, declarations.clone())),
        }
        self.rules
            .as_mut()
            .expect("stylesheets initialized")
            .set_pseudo_effect_declarations(node,pseudo,origin,&declarations)
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

    /// Capture the declaring tree for a non-CSS font-family reference such as
    /// Canvas. Scope chains share the same index-owned intern table as styles.
    pub fn font_reference_scope(&mut self,node:NodeId)->Result<Option<Arc<[NodeId]>>,LayoutError> {
        let root=self.document.root_node(node,false).map_err(|_|LayoutError::InvalidTree)?;
        let Some(_)=self.document.shadow_host(root).map_err(|_|LayoutError::InvalidTree)? else{return Ok(None);};
        self.refresh_rules()?;
        let scope=Some(root);
        self.rules.as_ref().expect("stylesheets initialized").font_scope_chain(&self.document,scope).map_err(LayoutError::Css)
    }

    pub fn computed_style(&mut self, node: NodeId) -> Result<Style, LayoutError> {
        self.computed_style_with_text(node,None)
    }
    pub fn view_transition_pseudo_style(&mut self, origin: NodeId,
        inherited: Option<&Style>, pseudo: css::PseudoElement, name: Option<&str>,
        text: &dyn TextShaper) -> Result<Style, LayoutError> {
        self.synchronize_form_state();
        self.refresh_rules()?;
        self.rules.as_ref().expect("stylesheets initialized")
            .compute_view_transition_style(&self.document,origin,inherited,pseudo,name,text)
            .map_err(LayoutError::Css)
    }
    pub fn computed_style_with_text(&mut self, node: NodeId, text:Option<&dyn TextShaper>) -> Result<Style, LayoutError> {
        self.synchronize_form_state();
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules()?;
        let rules = self.rules.as_ref().expect("stylesheets initialized");
        let layout_frame_id = self.frame_id;
        let key = ReadStyleKey {
            text_generation: text.map(|text|text.generation()),
            document_version: self.document.version(),
            rules_generation: self.rules_generation,
            validity_generation: self.validity_generation,
            interaction_generation: self.interaction_generation,
            animation_epoch: self.animation_epoch,
            environment: rules.environment,
            layout_frame_id,
        };
        if self.read_style_key != Some(key) {
            self.read_style_cache = css::StyleCache::default();
            self.read_style_key = Some(key);
        }
        let geometry = self.cached.as_ref().and_then(|frame| {
            (frame.document_version == self.document.version()
                && frame.width as f32 == rules.environment.width
                && frame.height as f32 == rules.environment.height)
                .then_some(&frame.geometry)
        });
        let cache = &mut self.read_style_cache;
        cache.prepare_text_context(text);
        let mut ancestors = Vec::new();
        let mut current = Some(node);
        let mut parent_style = None;
        let mut inherited_pending = false;
        while let Some(id) = current {
            if let Some(style) = cache.snapshot_style(id) {
                inherited_pending |= style.has_inherited_query_container_dependencies();
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
            let style=match text {Some(text)=>css::compute_node_cached_with_text(&self.document,id,parent_style.as_deref(),rules,cache,text),None=>css::compute_node_cached(&self.document,id,parent_style.as_deref(),rules,cache)}.map_err(LayoutError::Css)?;
            inherited_pending |= style.has_inherited_query_container_dependencies();
            parent_style = Some(cache.remember_snapshot_style(&self.document, id, style));
        }
        let mut style = parent_style
            .map(|style| (*style).clone())
            .ok_or(LayoutError::InvalidTree)?;
        if !inherited_pending && style.resolve_query_context_with_text(css::ContainerUnitContext::default(),text) {
            return Ok(style);
        }
        style_with_query_context(
            &self.document,
            node,
            rules,
            cache,
            geometry,
            text,
            false,
        )
        .map(|(style, _)| style)
    }

    pub fn computed_pseudo_style(&mut self, node: NodeId, pseudo: css::PseudoElement) -> Result<Style, LayoutError> {
        self.computed_pseudo_style_with_text(node,pseudo,None)
    }

    pub fn computed_pseudo_style_with_text(&mut self,node:NodeId,pseudo:css::PseudoElement,text:Option<&dyn TextShaper>)->Result<Style,LayoutError> {
        let ordinary=self.computed_style_with_text(node,text)?;
        let mut style=self.rules.as_ref().expect("stylesheets initialized")
            .compute_pseudo_computed(&self.document,node,&ordinary,pseudo,text).map_err(LayoutError::Css)?;
        let query=self.query_container_context(node)?;
        style.resolve_query_context_with_text(query,text);
        Ok(style)
    }

    /// Return the used container-unit context surrounding `node`. Eligible
    /// ancestors are selected in flat-tree order; an eligible box whose used
    /// size has not been laid out remains `Unknown` until the host flushes a
    /// frame. Axes with no eligible ancestor use the small-viewport fallback.
    pub fn query_container_context(
        &mut self,
        node: NodeId,
    ) -> Result<css::ContainerUnitContext, LayoutError> {
        self.synchronize_form_state();
        if node != self.document.root()
            && !matches!(self.document.kind(node), Ok(NodeKind::Element { .. }))
        {
            return Err(LayoutError::InvalidTree);
        }
        self.refresh_rules()?;
        let rules = self.rules.as_ref().expect("stylesheets initialized");
        let geometry = self.cached.as_ref().and_then(|frame| {
            (frame.document_version == self.document.version()
                && frame.width as f32 == rules.environment.width
                && frame.height as f32 == rules.environment.height)
                .then_some(&frame.geometry)
        });
        let mut cache = css::StyleCache::default();
        style_with_query_context(&self.document, node, rules, &mut cache, geometry,None,false)
            .map(|(_, context)| context)
    }

    /// Snapshot the computed CSS animation inputs and retained keyframes for
    /// the host animation adapter. Traversal covers document and shadow trees.
    pub fn animation_snapshot(&mut self) -> Result<Arc<AnimationSnapshot>, LayoutError> {
        let epoch=self.transition_input_epoch()?;
        if let Some(snapshot)=self.animation_snapshot.as_ref().filter(|snapshot|snapshot.epoch==epoch) {return Ok(snapshot.clone());}
        let source=self.transition_snapshot()?;
        self.animation_snapshot_from_transition(&source)
    }

    /// Both CSS effect families use the same connected, pseudo-qualified style
    /// snapshot. The compatibility entry point above builds it only once.
    pub fn animation_snapshot_from_transition(&mut self,source:&Arc<TransitionSnapshot>)->Result<Arc<AnimationSnapshot>,LayoutError> {
        if let Some(snapshot)=self.animation_snapshot.as_ref().filter(|snapshot|snapshot.source.ptr_eq(&Arc::downgrade(source))) {return Ok(snapshot.clone());}
        let rules=self.rules.as_ref().ok_or(LayoutError::InvalidTree)?;
        let mut nodes=Vec::new();
        for input in &source.nodes {
            if !input.was_rendered() || !input.style.animation.iter().any(Option::is_some) {continue;}
            let root=self.document.root_node(input.node,false).map_err(|_|LayoutError::InvalidTree)?;
            let scope=(root!=self.document.root()).then_some(root);
            nodes.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
            nodes.push((input.node,input.pseudo,scope,input.style.animation.clone()));
        }
        self.animation_generation=self.animation_generation.checked_add(1).ok_or(LayoutError::CommandLimit)?;
        let snapshot=Arc::new(AnimationSnapshot {generation:self.animation_generation,nodes,
            keyframes:rules.keyframes.clone(),source:Arc::downgrade(source),epoch:source.epoch});
        self.animation_snapshot=Some(snapshot.clone());Ok(snapshot)
    }

    pub fn sampled_effect_declarations(&self,origin:css::EffectOrigin)->impl Iterator<Item=(NodeId,Option<css::PseudoElement>,&[(String,String)])> {
        self.animated_styles.iter().filter(move |(_,_,candidate,_)|*candidate==origin)
            .map(|(node,pseudo,_,pairs)|(*node,*pseudo,pairs.as_slice()))
    }

    pub fn transition_input_epoch(&mut self) -> Result<TransitionInputEpoch,LayoutError> {
        self.synchronize_form_state();
        self.refresh_rules_only()?;
        Ok(TransitionInputEpoch {
            document_version:self.document.version(), rules_generation:self.rules_generation,
            font_generation:self.font_generation, validity_generation:self.validity_generation,
            interaction_generation:self.interaction_generation,
            environment:self.rules.as_ref().expect("stylesheets initialized").environment,
        })
    }

    /// One document walk at each real style-change event. Shared ancestor Arc
    /// styles preserve DOM inheritance and cached principal-parent provenance.
    fn push_transition_style(nodes:&mut Vec<TransitionStyleInput>,payload_bytes:&mut usize,input:TransitionStyleInput,quota_exceeded:&mut bool)->Result<(),LayoutError> {
        const LIMIT:usize=16*1024*1024;
        let charge=layout::checked_style_retained_bytes(&input.style).and_then(|bytes|
            input.eligibility.checked_retained_bytes().and_then(|eligibility|bytes.checked_add(eligibility)))
            .ok_or_else(||{*quota_exceeded=true;LayoutError::CommandLimit})?;
        let payload=payload_bytes.checked_add(charge).ok_or_else(||{*quota_exceeded=true;LayoutError::CommandLimit})?;
        let capacity=if nodes.len()==nodes.capacity() {nodes.capacity().max(4).checked_mul(2).ok_or_else(||{*quota_exceeded=true;LayoutError::CommandLimit})?}else{nodes.capacity()};
        if capacity.checked_mul(core::mem::size_of::<TransitionStyleInput>()).and_then(|bytes|bytes.checked_add(payload)).is_none_or(|bytes|bytes>LIMIT) {*quota_exceeded=true;return Err(LayoutError::CommandLimit);}
        nodes.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
        if nodes.capacity().checked_mul(core::mem::size_of::<TransitionStyleInput>()).and_then(|bytes|bytes.checked_add(payload)).is_none_or(|bytes|bytes>LIMIT) {*quota_exceeded=true;return Err(LayoutError::CommandLimit);}
        *payload_bytes=payload;nodes.push(input);Ok(())
    }

    pub fn transition_snapshot(&mut self) -> Result<Arc<TransitionSnapshot>,LayoutError> {
        self.transition_snapshot_with_text(None)
    }

    pub fn transition_snapshot_with_text(&mut self,text:Option<&dyn TextShaper>)->Result<Arc<TransitionSnapshot>,LayoutError> {
        let epoch=self.transition_input_epoch()?;
        let key=(epoch,text.map(TextShaper::generation),self.paint_revision);
        if self.rejected_transition_snapshot.as_deref()==Some(&key) {
            return Err(LayoutError::CommandLimit);
        }
        // Allocation/parse/text failures remain retryable; admission checks
        // alone set this flag, so an unchanged oversized tree is walked once.
        let mut quota_exceeded=false;
        let result=self.capture_transition_snapshot_with_text(text,epoch,&mut quota_exceeded,None,None);
        self.rejected_transition_snapshot=if quota_exceeded {Some(Box::new(key))} else {None};
        result
    }

    /// Capture current effect-underlying author provenance only for the supplied
    /// targets and their composed ancestors. This does not replace the ordered
    /// whole-document transition before-change snapshot or keep a persistent cache.
    pub fn effect_underlying_snapshot_with_text(&mut self,targets:&[(NodeId,Option<css::PseudoElement>)],
        text:Option<&dyn TextShaper>)->Result<Arc<TransitionSnapshot>,LayoutError>{
        const LIMIT:usize=16*1024*1024;
        let epoch=self.transition_input_epoch()?;
        let mut selection=EffectStyleSelection{nodes:Vec::new(),selected:alloc::collections::BTreeMap::new(),retained_bytes:0};
        let mut ancestors=Vec::new();
        for &(node,pseudo)in targets {
            if !matches!(self.document.kind(node),Ok(NodeKind::Element{..})){return Err(LayoutError::InvalidTree);}
            ancestors.clear();let mut current=Some(node);let mut depth=0;
            while let Some(id)=current {
                if selection.selected.contains_key(&id.key()){break;}
                if depth>=512{return Err(LayoutError::DepthLimit);}depth+=1;
                if matches!(self.document.kind(id),Ok(NodeKind::Element{..})){
                    ancestors.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;ancestors.push(id);
                }
                current=self.document.composed_parent(id).map_err(|_|LayoutError::InvalidTree)?;
            }
            for &id in ancestors.iter().rev(){
                let count=selection.nodes.len().checked_add(1).ok_or(LayoutError::CommandLimit)?;
                // Conservative per-entry admission includes BTree node slack,
                // the source-cache keys and both traversal-vector capacities.
                let capacity=if selection.nodes.len()==selection.nodes.capacity(){selection.nodes.capacity().max(4).checked_mul(2).ok_or(LayoutError::CommandLimit)?}else{selection.nodes.capacity()};
                let bytes=count.checked_mul(1024).and_then(|bytes|capacity.checked_mul(core::mem::size_of::<NodeId>()).and_then(|vector|bytes.checked_add(vector)))
                    .and_then(|bytes|ancestors.capacity().checked_mul(core::mem::size_of::<NodeId>()).and_then(|vector|bytes.checked_add(vector)))
                    .filter(|bytes|*bytes<=LIMIT).ok_or(LayoutError::CommandLimit)?;
                selection.nodes.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
                if selection.nodes.capacity()>capacity{return Err(LayoutError::CommandLimit);}
                selection.selected.insert(id.key(),0);selection.nodes.push(id);selection.retained_bytes=bytes;
            }
            if let Some(pseudo)=pseudo{*selection.selected.get_mut(&node.key()).ok_or(LayoutError::InvalidTree)?|=1u16<<(pseudo as u8);}
        }
        let mut quota_exceeded=false;
        self.capture_transition_snapshot_with_text(text,epoch,&mut quota_exceeded,Some(&selection),None)
    }

    fn capture_transition_snapshot_with_text(&mut self,text:Option<&dyn TextShaper>,epoch:TransitionInputEpoch,
        quota_exceeded:&mut bool,selection:Option<&EffectStyleSelection>,completed_geometry:Option<&layout::LayoutGeometry>)->Result<Arc<TransitionSnapshot>,LayoutError> {

        let rules=self.rules.as_ref().expect("stylesheets initialized");
        let geometry=completed_geometry.or_else(||self.cached.as_ref().map(|frame| &frame.geometry));
        let mut cache=match selection {Some(selection)=>css::StyleCache::selected(&selection.nodes).ok_or(LayoutError::CommandLimit)?,None=>css::StyleCache::default()};
        let root=self.document.root();
        let mut selected_index=0;
        let mut cursor=selection.map_or(Some(root),|selection|selection.nodes.first().copied());
        let mut nodes=Vec::new();
        let mut payload_bytes=selection.map_or(0,|selection|selection.retained_bytes);
        let mut eligibility=alloc::collections::BTreeMap::new();
        let mut rendered_nodes=alloc::collections::BTreeMap::<u128,bool>::new();
        while let Some(node)=cursor {
            if matches!(self.document.kind(node),Ok(NodeKind::Element{..})) {
                let mut ancestors=Vec::new();
                let mut current=Some(node);
                let mut parent=None;
                while let Some(id)=current {
                    if let Some(style)=cache.node_style(id) { parent=Some(style); break; }
                    if ancestors.len() >= 512 { return Err(LayoutError::DepthLimit); }
                    ancestors.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
                    ancestors.push(id);
                    current=self.document.composed_parent(id).map_err(|_|LayoutError::InvalidTree)?;
                }
                for id in ancestors.into_iter().rev() {
                    if !matches!(self.document.kind(id),Ok(NodeKind::Element{..})) { continue; }
                    let parent_rendered=parent.as_ref().is_none_or(|parent:&Arc<Style>|parent.display != css::Display::None);
                    let (computed,eligible)=rules.compute_transition_snapshot_cached(&self.document,id,parent.as_deref(),None,text,
                        &mut cache,false).map_err(LayoutError::Css)?;
                    parent=Some(computed); eligibility.insert(id.key(),eligible);
                    let ancestor_rendered=self.document.composed_parent(id).ok().flatten()
                        .and_then(|id|rendered_nodes.get(&id.key()).copied()).unwrap_or(parent_rendered);
                    rendered_nodes.insert(id.key(),ancestor_rendered && parent.as_ref().is_some_and(|style|style.display != css::Display::None));
                }
                let mut style=parent.ok_or(LayoutError::InvalidTree)?;
                let mut query=None;
                if style.has_query_container_dependencies() || style.has_inherited_query_container_dependencies() {
                    let (resolved,context)=style_with_query_context(&self.document,node,rules,&mut cache,geometry,text,true)?;
                    query=Some(context);style=cache.replace_snapshot_style(&self.document,node,resolved);
                }
                let rendered=self.document.is_connected_element(node) && rendered_nodes.get(&node.key()).copied().unwrap_or(false);
                let mut eligible=eligibility.remove(&node.key()).unwrap_or_default();
                if let Some(context)=query {eligible.set_query_context(context);}
                let mut inherited_parent=self.document.composed_parent(node).map_err(|_|LayoutError::InvalidTree)?;
                while let Some(id)=inherited_parent {
                    if matches!(self.document.kind(id),Ok(NodeKind::Element{..})) {break;}
                    inherited_parent=self.document.composed_parent(id).map_err(|_|LayoutError::InvalidTree)?;
                }
                Self::push_transition_style(&mut nodes,&mut payload_bytes,TransitionStyleInput {node,parent:inherited_parent,pseudo:None,style:style.clone(),rendered:Cell::new(rendered),
                    eligibility:eligible},quota_exceeded)?;
                {
                    let quote=matches!(self.document.kind(node),Ok(NodeKind::Element {name,namespace:crate::Namespace::Html,..}) if name == "q");
                    for pseudo in [css::PseudoElement::Marker,css::PseudoElement::Before,css::PseudoElement::After,
                        css::PseudoElement::BeforeMarker,css::PseudoElement::AfterMarker,
                        css::PseudoElement::FirstLine,css::PseudoElement::FirstLetter,css::PseudoElement::Highlight,css::PseudoElement::Placeholder,
                        css::PseudoElement::ViewTransition,css::PseudoElement::ViewTransitionGroup,css::PseudoElement::ViewTransitionImagePair,
                        css::PseudoElement::ViewTransitionOld,css::PseudoElement::ViewTransitionNew] {
                        let requested=selection.is_some_and(|selection|selection.selected.get(&node.key()).is_some_and(|bits|bits&(1u16<<(pseudo as u8))!=0));
                        if selection.is_some()&&!requested{continue;}
                        if selection.is_none()&&!matches!(pseudo,css::PseudoElement::Marker|css::PseudoElement::Before|css::PseudoElement::After|css::PseudoElement::BeforeMarker|css::PseudoElement::AfterMarker){continue;}
                        let generated_origin = pseudo.marker_origin().map(|parent| rules.compute_pseudo_cached(
                            &self.document, node, &style, parent, text, &mut cache)).transpose().map_err(LayoutError::Css)?.flatten();
                        let pseudo_origin = generated_origin.as_ref().map_or(style.as_ref(), |parent| &parent.style);
                        let origin_present = pseudo.marker_origin().is_none() || generated_origin.as_ref()
                            .is_some_and(|parent| parent.style.display != css::Display::None && parent.style.is_list_item());
                        if !requested && !origin_present {continue;}
                        if !requested && !rules.has_transition_pseudo_rules(pseudo) && !(pseudo.is_marker() && pseudo_origin.is_list_item()) && !(quote && !pseudo.is_marker()) {continue;}
                        let (mut generated,mut eligible)=rules.compute_transition_snapshot_cached(&self.document,node,Some(pseudo_origin),Some(pseudo),text,
                            &mut cache,false).map_err(LayoutError::Css)?;
                        if generated.has_query_container_dependencies() || generated.has_inherited_query_container_dependencies() {
                            let context=match query {Some(context)=>context,None=>style_with_query_context(&self.document,node,rules,&mut cache,geometry,text,true)?.1};
                            Arc::make_mut(&mut generated).resolve_query_context_with_text(context,text);eligible.set_query_context(context);
                        }
                        let generated_present=match generated.generated_content() {
                            css::GeneratedContent::Items(_) => true,
                            css::GeneratedContent::Normal => pseudo.is_marker() && origin_present && pseudo_origin.is_list_item(),
                            css::GeneratedContent::None => false,
                        };
                        if generated_present || requested {
                            Self::push_transition_style(&mut nodes,&mut payload_bytes,TransitionStyleInput {node,parent:Some(node),pseudo:Some(pseudo),rendered:Cell::new(rendered && origin_present && (generated.display != css::Display::None || pseudo.is_marker()) && (generated_present || !matches!(pseudo,css::PseudoElement::Marker|css::PseudoElement::Before|css::PseudoElement::After|css::PseudoElement::BeforeMarker|css::PseudoElement::AfterMarker))),style:generated,eligibility:eligible},quota_exceeded)?;
                        }
                    }
                }
            }
            cursor=if let Some(selection)=selection{selected_index+=1;selection.nodes.get(selected_index).copied()}
                else{crate::selector::next_shadow_including_descendant(&self.document,root,node).map_err(|_|LayoutError::InvalidTree)?};
        }
        Ok(Arc::new(TransitionSnapshot {epoch,nodes}))
    }

    pub fn transition_starting_style(&mut self,node:NodeId,pseudo:Option<css::PseudoElement>,
        parent:Option<&Style>) -> Result<Arc<Style>,LayoutError> {
        self.transition_starting_style_with_text(node,pseudo,parent,None)
    }

    pub fn transition_starting_style_with_text(&mut self,node:NodeId,pseudo:Option<css::PseudoElement>,parent:Option<&Style>,text:Option<&dyn TextShaper>)->Result<Arc<Style>,LayoutError> {
        self.refresh_rules_only()?;
        self.rules.as_ref().expect("stylesheets initialized").compute_transition_style_cached(
            &self.document,node,parent,pseudo,text,&mut css::StyleCache::default(),true).map_err(LayoutError::Css)
    }

    fn refresh_rules(&mut self) -> Result<(), LayoutError> {
        self.refresh_rules_only()?;
        self.flush_animation_changes();
        Ok(())
    }

    fn refresh_rules_only(&mut self) -> Result<(), LayoutError> {
        self.reclaim_stale_stylesheet_sources();
        let version = self.document.version();
        let metadata_changed=self.rules.is_none() || self.rules_version!=version && self.document.mutations().iter().any(|mutation|match &mutation.kind {
            MutationKind::FullRebuild=>true,
            MutationKind::Attribute(name) if matches!(name.as_str(),"name"|"content")=>matches!(self.document.kind(mutation.target),Ok(NodeKind::Element{namespace:crate::Namespace::Html,name,..}) if crate::svg::local_name(name)=="meta"),
            MutationKind::Tree{added,removed,..}=>added.is_some_and(|node|css::color_scheme::subtree_has_meta(&self.document,node)) || removed.is_some_and(|node|css::color_scheme::subtree_has_meta(&self.document,node)),
            _=>false,
        });
        if metadata_changed {
            let support=css::color_scheme::document_support(&self.document).map_err(|_|LayoutError::InvalidTree)?;
            let mut environment=self.rules.as_ref().map(|rules|rules.environment).or(self.style_environment).unwrap_or_default();
            if environment.color_schemes.page_support!=support {
                environment.color_schemes.page_support=support;
                self.layout_cache.clear();
                self.style_environment=Some(environment);self.rules=None;
                self.style_cache.borrow_mut().clear();self.invalidate_paint();
            }
        }
        if self.rules.is_none() || self.rules_version!=version {
        // Parser/pragma callbacks publish the actual sheet flag without reentering
        // Session. Project it at this shared safe boundary before CSS compilation.
        for (owner,_,weak) in &self.stylesheet_graph_leases {
            let Some(lease)=weak.upgrade().filter(|lease|!lease.is_detached()) else {continue;};
            let disabled=lease.disabled();
            if let Some(state)=self.stylesheet_sources.iter_mut().find(|source|source.owner==*owner).and_then(|source|source.link_state.as_mut()) {
                if state.disabled!=disabled {state.disabled=disabled;self.rules=None;}
            }else if self.disabled_stylesheets.contains(owner)!=disabled {
                if disabled {self.disabled_stylesheets.try_reserve(1).map_err(|_|LayoutError::InvalidTree)?;self.disabled_stylesheets.push(*owner);}
                else {self.disabled_stylesheets.retain(|node|node!=owner);}
                self.rules=None;
            }
        }
        }

        self.animated_styles
            .retain(|(node, _, _, _)| self.document.kind(*node).is_ok());
        self.stylesheet_sources
            .retain(|source| self.document.kind(source.owner).is_ok());
        self.cssom_rule_overrides.retain(|retained| match retained.owner {
            css::StylesheetIdentity::Dom(node) => self.document.kind(node).is_ok(),
            css::StylesheetIdentity::Adopted { scope, .. } => scope.is_none_or(|node| self.document.kind(node).is_ok()),
        });
        self.cssom_topology_overrides.retain(|retained| match retained.owner {
            css::StylesheetIdentity::Dom(node) => self.document.kind(node).is_ok(),
            css::StylesheetIdentity::Adopted { scope, .. } => scope.is_none_or(|node| self.document.kind(node).is_ok()),
        });
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
                let (mut rules, mut font_faces) = layout::stylesheets_with_source_topologies(
                    &self.document,
                    &self.linked_stylesheets,
                    &self.stylesheet_sources,
                    &self.adopted_stylesheets,
                    self.document_base_url.clone(),
                    environment,
                    &self.cssom_topology_overrides,
                    &self.cssom_stylesheet_texts,
                    &self.disabled_stylesheets,
                    self.preferred_stylesheet_set.as_deref(),
                )?;
                rules.apply_cssom_rule_overrides(&self.cssom_rule_overrides).map_err(LayoutError::Css)?;
                rules.environment = environment;
                self.animated_styles
                    .retain(|(node, _, _, _)| self.document.kind(*node).is_ok());
                for (node, pseudo, origin, declarations) in &self.animated_styles {
                    rules
                        .set_pseudo_effect_declarations(*node,*pseudo,*origin,declarations)
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
            let Some(bounds) = layout::transformed_bounds(hit.rect, transforms) else {
                continue;
            };
            for clip in frame
                .geometry
                .rounded_clips
                .iter()
                .filter(|clip| clip.hits.contains(&index))
            {
                let transforms = frame.geometry.clip_transforms(clip);
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
                let transforms = frame.geometry.clip_transforms(clip);
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

    /// Unclipped fragment corners in viewport coordinates. Keeping the corners
    /// through nested coordinate systems avoids enlarging rotated boxes twice.
    pub fn client_fragment_corners(&self, node: NodeId) -> impl Iterator<Item = [(f32, f32); 4]> + '_ {
        // Follow getBoundingClientRect's empty-fragment rule without retaining
        // an intermediate list. Dimensions are measured after transforms.
        let has_full_box = self.client_rect_iter(node).any(|rect| rect.width != 0.0 && rect.height != 0.0);
        let mut first=true;
        self.cached.as_ref().filter(|frame| frame.document_version == self.document.version()
            && self.document.kind(node).is_ok()).into_iter().flat_map(move |frame| {
            frame.geometry.hits.iter().enumerate()
                .filter(move |(_,hit)|hit.node==node&&!hit.virtual_generated&&hit.rect.is_valid())
                .flat_map(move |(index,_)|frame.geometry.hit_rects(index).iter().map(move |rect| {
                let mut corners = [(rect.x, rect.y), (rect.x + rect.width, rect.y),
                    (rect.x, rect.y + rect.height), (rect.x + rect.width, rect.y + rect.height)];
                for transform in frame.geometry.transforms.iter().filter(|transform| transform.hits.contains(&index)) {
                    for point in &mut corners { *point = transform.matrix.apply(point.0, point.1); }
                }
                corners
            }))
        }).filter_map(move |corners| {
                if !corners.iter().all(|(x, y)| x.is_finite() && y.is_finite()) { return None; }
                let first_fragment = core::mem::replace(&mut first, false);
                if !has_full_box { return first_fragment.then_some(corners); }
                let (mut left, mut top, mut right, mut bottom) = (corners[0].0, corners[0].1, corners[0].0, corners[0].1);
                for &(x,y) in &corners[1..] { left=left.min(x);top=top.min(y);right=right.max(x);bottom=bottom.max(y); }
                (left != right || top != bottom).then_some(corners)
        })
    }

    /// Map coordinates in an embedded viewport to its owner's content box.
    pub fn content_viewport_transform(&mut self, node: NodeId) -> Option<crate::paint::Affine> {
        let style = self.computed_style(node).ok()?;
        let frame = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version()
            && self.document.kind(node).is_ok())?;
        let mut hits = frame.geometry.hits.iter().enumerate()
            .filter(|(_,hit)|hit.node == node && !hit.virtual_generated && hit.rect.is_valid());
        let (index, first) = hits.next()?;
        let mut rect = first.rect;
        for (_,hit) in hits {
            let x=rect.x.min(hit.rect.x);let y=rect.y.min(hit.rect.y);
            rect=Rect{x,y,width:(rect.x+rect.width).max(hit.rect.x+hit.rect.width)-x,
                height:(rect.y+rect.height).max(hit.rect.y+hit.rect.height)-y};
        }
        let style = Self::resolve_box_style(style, first.percentage_basis);
        let border = style.used_border_widths();
        let padding = style.padding_sides();
        let translation = crate::paint::Affine { e: rect.x + border[3] + padding[3],
            f: rect.y + border[0] + padding[0], ..crate::paint::Affine::IDENTITY };
        let mut matrix = translation;
        for transform in frame.geometry.transforms.iter().filter(|transform| transform.hits.contains(&index)) {
            matrix = transform.matrix.then(matrix);
        }
        matrix.is_finite().then_some(matrix)
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
                    .filter(move |(_,hit)|hit.node==node&&!hit.virtual_generated&&hit.rect.is_valid())
                    .flat_map(move |(index, _hit)| frame.geometry.hit_rects(index).iter().filter_map(move |rect| {
                        let transforms = frame
                            .geometry
                            .transforms
                            .iter()
                            .filter(|transform| transform.hits.contains(&index))
                            .map(|transform| transform.matrix);
                        layout::transformed_bounds(*rect, transforms)
                    }))
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
        self.layout_rect_with_replacement(node).map(|(rect, _)| rect)
    }

    /// Untransformed principal geometry and its actual replacement policy in
    /// one fragment visit. CSSOM must not guess resource state from tag names.
    pub fn layout_rect_with_replacement(&self, node: NodeId) -> Option<(Rect, bool)> {
        self.effect_layout_box(node, None)
    }

    /// Actual reference geometry produced by the same layout as paint/hits.
    /// SVG records are selected local user boxes; tables record their wrapper.
    pub fn layout_transform_reference_box(&self,node:NodeId,pseudo:Option<css::PseudoElement>)->Option<(Rect,bool)>{
        let frame=self.cached.as_ref().filter(|frame|frame.document_version==self.document.version() && self.document.kind(node).is_ok())?;
        frame.geometry.transform_reference_box(node,pseudo)
    }

    /// The current principal box's used formatting, retained with its actual
    /// geometry. Host-language blockification does not change computed display.
    pub fn layout_used_display(&self, node: NodeId) -> Option<css::Display> {
        let frame=self.cached.as_ref().filter(|frame| frame.document_version==self.document.version()
            && self.document.kind(node).is_ok())?;
        frame.geometry.hits.iter().find(|hit| hit.node==node && !hit.virtual_generated
            && hit.pseudo.is_none() && hit.rect.is_valid()).and_then(|hit|hit.used_display)
    }

    /// Actual generated box fragments; they remain excluded from DOM element geometry.
    pub fn pseudo_layout_rect(&self,node:NodeId,pseudo:css::PseudoElement)->Option<Rect> {
        self.effect_layout_box(node,Some(pseudo)).map(|(rect, _)| rect)
    }

    fn effect_layout_box(&self,node:NodeId,pseudo:Option<css::PseudoElement>)->Option<(Rect,bool)> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        frame.geometry.effect_box(node,pseudo)
    }

    /// Exact percentage basis recorded by the layout that produced this box.
    /// Grid areas and out-of-flow boxes may differ from the DOM parent box.
    pub fn layout_percentage_basis(&self, node: NodeId) -> Option<(f32, Option<f32>)> {
        self.target_layout_percentage_basis(node,None)
    }

    /// The containing block belongs to the actual generated box. An absent
    /// pseudo has no used-value basis even when its originating element does.
    pub fn pseudo_layout_percentage_basis(&self,node:NodeId,pseudo:css::PseudoElement)->Option<(f32,Option<f32>)> {
        self.target_layout_percentage_basis(node,Some(pseudo))
    }

    fn target_layout_percentage_basis(&self,node:NodeId,pseudo:Option<css::PseudoElement>)->Option<(f32,Option<f32>)> {
        let frame = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version() && self.document.kind(node).is_ok())?;
        frame.geometry.hits.iter().find(|hit| hit.node == node && match pseudo {
            Some(pseudo)=>hit.pseudo==Some(pseudo),
            None=>!hit.virtual_generated && hit.pseudo.is_none(),
        } && hit.rect.is_valid()).and_then(|hit| hit.percentage_basis)
    }

    /// Resolve box-edge percentages against the actual containing block used
    /// by the retained formatter, including grid and positioned boxes.
    pub fn used_box_style(&mut self, node: NodeId) -> Result<Style, LayoutError> {
        let mut style=self.computed_style(node)?;
        layout::apply_widget_layout_style(&self.document, node, &mut style);
        if let Some(display)=self.layout_used_display(node) {style.display=display;}
        Ok(Self::resolve_box_style(style, self.layout_percentage_basis(node)))
    }

    fn resolve_box_style(style: Style, basis: Option<(f32, Option<f32>)>) -> Style {
        match basis {
            Some((width,height))=>style.resolve_percentages(width,height),
            None=>style,
        }
    }

    /// Fresh, untransformed concrete object width for HTML auto-sizes. Used
    /// box edges come from the same actual percentage basis as the formatter.
    pub fn image_auto_sizes_width(&mut self,node:NodeId)->Option<f32> {
        if !crate::responsive_images::allows_auto_sizes(&self.document,node){return None;}
        let rect=self.layout_rect(node)?;let style=self.used_box_style(node).ok()?;
        let border=style.used_border_widths();let padding=style.padding_sides();
        Some((rect.width-border[1]-border[3]-padding[1]-padding[3]).max(0.0))
    }

    /// Used physical margin values for a rendered box whose formatter resolved
    /// authored `auto` margins or adjusted a side. Sparse values share the
    /// layout frame's version and are replayed with retained boxes.
    pub fn layout_used_margins(&self, node: NodeId) -> Option<[f32; 4]> {
        let frame = self.cached.as_ref().filter(|frame| {
            frame.document_version == self.document.version() && self.document.kind(node).is_ok()
        })?;
        // A used-margin entry is recorded only while laying out a real box
        // (never display:none/contents/generated boxes), and retained
        // fragments replay it with the same document-version guard. It is
        // therefore already the sparse proof that this node had a live box;
        // scanning every hit region for each CSSOM margin read would add an
        // O(hit-count) lookup to an O(log used-margins) path.
        frame.geometry.used_margins_for(node)
    }

    /// Measured Grid/Lanes track breadths from the current untransformed frame.
    /// The two shared arrays are retained only for actual grid roots; cloning
    /// their handles permits serialization after the Session borrow is dropped.
    pub fn layout_grid_tracks(&self, node: NodeId) -> Option<(Arc<[f32]>, Arc<[f32]>, Arc<[css::GridNamedLine]>, Arc<[css::GridNamedLine]>)> {
        let frame = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version() && self.document.kind(node).is_ok())?;
        let tracks = frame.geometry.hits.iter().find(|hit| hit.node == node && !hit.virtual_generated && hit.rect.is_valid())?.grid_tracks.as_ref()?;
        Some((tracks.columns.clone(), tracks.rows.clone(), tracks.column_names.clone(), tracks.row_names.clone()))
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
            let Some(_) = clip.hits.clone().find(|index| {
                frame
                    .geometry
                    .hits
                    .get(*index)
                    .is_some_and(|hit| hit.node == node && !hit.virtual_generated)
            }) else {
                continue;
            };
            let transforms=frame.geometry.clip_transforms(clip).collect();
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
                let transforms = frame.geometry.clip_transforms(clip);
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
            .map(|extent| (extent.max_x-extent.min_x, extent.max_y-extent.min_y))
    }

    pub fn used_transform_timeline_source(&self,node:NodeId,pseudo:Option<css::PseudoElement>,style:&Style)->Option<Arc<str>>{
        let source=style.transform_timeline_source()?;
        let frame=self.cached.as_ref().filter(|frame|frame.document_version==self.document.version()&&self.document.kind(node).is_ok())?;
        frame.geometry.transform_timeline_values.as_deref()?.get_value(node,pseudo,source).map(|value|value.used.clone())
    }

    pub(crate) fn completed_geometry(&self)->Option<&layout::LayoutGeometry>{
        self.cached.as_ref().filter(|frame|frame.document_version==self.document.version()).map(|frame|&frame.geometry)
    }

    /// Signed CSSOM scroll limits from the actual overflow and scroll origin.
    pub fn scroll_bounds(&self, node: NodeId) -> Option<(f32,f32,f32,f32)> {
        let frame=self.cached.as_ref().filter(|frame|
            frame.document_version==self.document.version() && self.document.kind(node).is_ok())?;
        frame.geometry.scroll_bounds(node)
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

    /// The local CSS-pixel viewport of the actual scrolling box, independent
    /// of transforms and its owner element's principal border-box dimensions.
    pub fn scrollport_size(&self, node: NodeId) -> Option<(f32, f32)> {
        let frame = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version()
            && self.document.kind(node).is_ok())?;
        let port = frame.geometry.scroll_ports.iter().find(|port| port.node == node)?;
        port.rect.is_valid().then_some((port.rect.width, port.rect.height))
    }

    /// Scroll offsets use the box's untransformed CSS-pixel axes, rather than
    /// the axis-aligned bounds of its transformed viewport rectangle.
    pub fn scrollport_coordinate_space(&self, node: NodeId) -> Option<(Rect, crate::paint::Affine)> {
        let frame = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version()
            && self.document.kind(node).is_ok())?;
        frame.geometry.scrollport_coordinate_space(node)
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

    /// Layout can clamp existing positions when a scrollable region shrinks or
    /// changes its origin. The embedder admits notifications outside layout.
    pub fn pending_normalized_scroll_event(&self) -> Option<NodeId> {
        self.normalized_scroll_events.last().copied()
    }

    pub fn acknowledge_normalized_scroll_event(&mut self, node: NodeId) {
        self.normalized_scroll_events.retain(|pending|*pending!=node);
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
                .map(|extent| (extent.min_x,extent.max_x,extent.min_y,extent.max_y))
        });
        // The viewport records no extent when its content fits, but a stale offset
        // from earlier, larger content must still be clamped back to zero.
        let Some((min_x,max_x,min_y,max_y)) = extent.or_else(|| {
            (node == self.document.root() && self.cached.is_some()).then_some((0.0,0.0,0.0,0.0))
        }) else {
            return Ok(false);
        };
        let (x,y)=(x_css.clamp(min_x,max_x),y_css.clamp(min_y,max_y));
        self.commit_scroll_offset(node, x, y)
    }

    /// Clamp and admit all samples before changing geometry. Sorting only the
    /// caller's scalar scratch records avoids an extra per-frame allocation
    /// and a scroll-container scan for every sample.
    pub fn set_scroll_offsets(&mut self, updates: &mut [ScrollUpdate]) -> Result<(), LayoutError> {
        for (order, update) in updates.iter_mut().enumerate() {
            self.document.kind(update.node).map_err(|_| LayoutError::InvalidTree)?;
            if !update.x.is_finite() || !update.y.is_finite() { return Err(LayoutError::InvalidTree); }
            update.changed = false; update.admitted = false; update.order = order;
        }
        updates.sort_unstable_by_key(|update| update.node.key());
        if let Some(frame) = self.cached.as_ref().filter(|frame| frame.document_version == self.document.version()) {
            let root_extent = frame.geometry.scroll_extents.iter().find(|extent| extent.node == self.document.root());
            for update in updates.iter_mut().filter(|update| update.node == self.document.root()) {
                if let Some(extent) = root_extent {
                    update.x = update.x.clamp(extent.min_x, extent.max_x);
                    update.y = update.y.clamp(extent.min_y, extent.max_y);
                } else { update.x = 0.0; update.y = 0.0; }
                update.admitted = true;
            }
            for extent in &frame.geometry.scroll_extents {
                let key = extent.node.key();
                let mut index = updates.partition_point(|update| update.node.key() < key);
                while let Some(update) = updates.get_mut(index).filter(|update| update.node == extent.node) {
                    update.x = update.x.clamp(extent.min_x, extent.max_x);
                    update.y = update.y.clamp(extent.min_y, extent.max_y);
                    update.admitted = true;
                    index += 1;
                }
            }
        }
        // Reserve only new distinct boxes. Repeated animation samples of
        // existing boxes must not expand the persistent offset vector.
        let mut previous = None;
        let mut additional = 0usize;
        for update in updates.iter().filter(|update| update.admitted) {
            if previous != Some(update.node) {
                additional += 1;
                previous = Some(update.node);
            }
        }
        for scroll in &self.scrolls {
            let index = updates.partition_point(|update| update.node.key() < scroll.node.key());
            if updates.get(index).is_some_and(|update| update.node == scroll.node && update.admitted) {
                additional -= 1;
            }
        }
        updates.sort_unstable_by_key(|update| update.order);
        self.scrolls.try_reserve(additional).map_err(|_| LayoutError::CommandLimit)?;
        for update in updates.iter_mut().filter(|update| update.admitted) {
            update.changed = self.commit_scroll_offset(update.node, update.x, update.y)?;
        }
        Ok(())
    }

    fn commit_scroll_offset(&mut self, node: NodeId, x: f32, y: f32) -> Result<bool, LayoutError> {
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
            if frame.geometry.transform_timeline_values.is_some(){return false;}
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

    /// Request text boxes from the ordinary formatter. Ordinary rendering
    /// retains no source-text sidecar until a consumer requests one.
    pub fn request_rendered_text_boxes(&mut self) {
        self.rendered_text_requested = true;
    }

    pub fn finish_rendered_text_boxes(&mut self) {
        self.rendered_text_requested = false;
    }

    pub(crate) fn rendered_text_boxes(&self) -> Option<&[layout::RenderedTextBox]> {
        let frame = self.cached.as_ref()?;
        (frame.document_version == self.document.version() && frame.geometry.collect_rendered_text)
            .then_some(frame.geometry.rendered_text_boxes.as_slice())
    }

    pub(crate) fn rendered_table_member(&self, node: NodeId, role: css::Display) -> Option<layout::RenderedTableMember> {
        let frame = self.cached.as_ref()?;
        if frame.document_version != self.document.version() || !frame.geometry.collect_rendered_text { return None; }
        let members = &frame.geometry.rendered_table_members;
        let start = members.partition_point(|member| member.node.key() < node.key());
        members[start..].iter().take_while(|member| member.node == node).find(|member| member.role == role).copied()
    }

    /// The embedder's ordinary display-list entry points can also serve a
    /// forced CSSOM layout. Preserve that phase while the existing callback
    /// runs, so it cannot admit a rendering-opportunity stale timeline update.
    fn sample_transform_timeline_values(&mut self,geometry:&layout::LayoutGeometry,text:&dyn TextShaper)->Result<bool,LayoutError>{
        use crate::animation::{ProgressRange,progress_timelines as timelines};
        use css::typed_numeric::{NumericType,NumericValue,NumericUnit};
        let Some(sources)=geometry.transform_timeline_values.as_deref() else{
            let changed=self.timeline_transform_values.is_some();self.timeline_transform_values=None;return Ok(changed);
        };
        let epoch=self.transition_input_epoch()?;let mut quota=false;
        let source=self.capture_transition_snapshot_with_text(Some(text),epoch,&mut quota,None,Some(geometry))?;
        let snapshot=self.animation_snapshot_from_transition(&source)?;
        let mut values=timelines::TransformTimelineValues::default();let mut changed=false;
        for input in sources.values.values() {
            let root=self.document.root_node(input.node,false).map_err(|_|LayoutError::InvalidTree)?;
            let scope=(root!=self.document.root()).then_some(root);
            let mut sample=|name:&str,absolute:Option<NumericType>|->Option<NumericValue>{
                let binding=timelines::resolve(self,&snapshot,input.node,scope,name,ProgressRange::parse("normal")?).ok()?;
                let sampled=binding.source.and_then(|node|{
                    let style_node=if node==self.document.root(){self.document.document_element_at(node).ok().flatten()?}else{node};
                    let style=source.nodes.iter().find(|input|input.node==style_node&&input.pseudo.is_none())?.style.as_ref();
                    timelines::sample_completed_geometry(&binding,style.logical_sides(),geometry,self.scroll_offset(node))
                });
                let Some(sampled)=sampled else{return Some(NumericValue {value:0.0,unit:NumericUnit::Number});};
                Some(match absolute {
                    None=>NumericValue {value:sampled.progress,unit:NumericUnit::Number},
                    Some(kind) if kind==NumericType::from_unit(NumericUnit::Px)=>NumericValue {value:sampled.position,unit:NumericUnit::Px},
                    // CSS scroll/view timelines have length coordinates. A
                    // time-typed absolute map cannot use their length progress.
                    _=>return None,
                })
            };
            let used:Arc<str>=Arc::from(css::typed_transforms::resolve_timeline_progress(&input.source,&mut sample).unwrap_or_else(||String::from("none")));
            changed|=used.as_ref()!=input.used.as_ref();
            values.insert(timelines::TransformTimelineValue {node:input.node,pseudo:input.pseudo,source:input.source.clone(),used})?;
        }
        self.timeline_transform_values=Some(Arc::new(values));Ok(changed)
    }

    pub fn with_forced_layout<R>(&mut self,callback:impl FnOnce(&mut Self)->R)->R {
        let previous=self.forced_layout;self.forced_layout=true;
        let result=callback(self);self.forced_layout=previous;result
    }

    pub fn display_list(
        &mut self,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
    ) -> Result<&DisplayList, LayoutError> {
        self.render(width, height, text, None)?;
        Ok(self.capture_overlay.as_ref().unwrap_or(&self.cached.as_ref().expect("rendered frame").list))
    }

    pub fn display_list_with_images(
        &mut self,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
        images: &dyn ImageResolver,
    ) -> Result<&DisplayList, LayoutError> {
        self.render(width, height, text, Some(images))?;
        Ok(self.capture_overlay.as_ref().unwrap_or(&self.cached.as_ref().expect("rendered frame").list))
    }

    /// Capture the normal viewport without recursively capturing a presentation
    /// overlay. Layout, scroll offsets, fonts and image resources stay shared.
    pub fn capture_display_list(
        &mut self, request: &crate::render_capture::RenderCaptureRequest,
        text: &dyn TextShaper, images: Option<&dyn ImageResolver>,
    ) -> Result<DisplayList, LayoutError> {
        match request.target {
            crate::render_capture::RenderCaptureTarget::Viewport =>
                self.render(request.width, request.height, text, images).cloned(),
        }
    }

    pub fn set_capture_overlay(&mut self, overlay: Option<DisplayList>) {
        if self.capture_overlay != overlay {
            self.capture_overlay = overlay;
            self.paint_revision = self.paint_revision.wrapping_add(1);
        }
    }

    /// Build an isolated drawable subtree using this document's styles and resources.
    /// The normal document frame and its hit geometry remain unchanged.
    pub fn element_snapshot_display_list(
        &mut self,
        root: NodeId,
        width: u32,
        height: u32,
        text: &dyn TextShaper,
        images: Option<&dyn ImageResolver>,
    ) -> Result<(DisplayList, Rect), LayoutError> {
        let (viewport_width,viewport_height)=self.cached.as_ref().map(|frame|(frame.width,frame.height)).unwrap_or((width,height));
        self.render(viewport_width, viewport_height, text, images)?;
        let mut geometry = layout::LayoutGeometry::default();
        let node_images = NodeImages { color_schemes:self.media_environment().color_schemes, svg_fragment:self.svg_fragment.as_deref(), svg_image_viewport:self.svg_image_viewport.as_deref().copied(), paint_definitions: &self.paint_definitions, paint_registration_revision:self.rules_generation, bitmaps: &self.node_bitmaps, embedded:&self.embedded_bitmaps, objects: &self.object_representations, fallback: images };
        let safe_images = SnapshotImages(&node_images,&self.document);
        let styles = core::cell::RefCell::new(css::StyleCache::default());
        let list = layout::display_list_with_snapshot_mode(
            &self.document, width, height, text,
            self.rules.as_ref().expect("stylesheets initialized"),
            Some(&safe_images), Some(&mut geometry), &self.scrolls, &styles,
            None, None, None, Some(root),
        )?;
        let rect = geometry.hits.iter().find(|hit| hit.node == root && !hit.virtual_generated)
            .map(|hit| hit.rect).unwrap_or(Rect { x: 0.0, y: 0.0, width: 0.0, height: 0.0 });
        Ok((list, rect))
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
        self.normalized_scroll_events.retain(|node|self.document.kind(*node).is_ok());
        self.node_bitmaps
            .retain(|(node, _, _)| self.document.kind(*node).is_ok());
        self.object_representations.retain(|(node, _, _)| self.document.kind(*node).is_ok());
        let version = self.document.version();
        let image_generation = images.map(ImageResolver::generation);
        if self.cached.as_ref().is_some_and(|cached| {
            cached.width != width
                || cached.height != height
                || cached.image_generation != image_generation
        }) {
            self.layout_cache.clear();
        }
        let fresh = (self.forced_layout||!self.timeline_layout_stale)&&self.cached.as_ref().is_some_and(|cached| {
            cached.document_version == version
                && cached.width == width
                && cached.height == height
                && cached.image_generation == image_generation
                && (!self.rendered_text_requested || cached.geometry.collect_rendered_text)
        });
        if !fresh {
            self.refresh_rules()?;
            let rules = self.rules.as_mut().expect("stylesheets initialized");
            rules.environment = css::MediaEnvironment {
                width: width as f32,
                height: height as f32,
                ..rules.environment
            };
            let canvas_background=if self.canvas_background_is_default {
                let environment=self.media_environment();
                let scheme=if let Some(root)=crate::selector::document_element(&self.document) {
                    self.computed_style_with_text(root,Some(text))?.used_color_scheme(environment).scheme
                }else {environment.color_schemes.page().scheme};
                if self.embedding_color_scheme==Some(scheme) {None}else{css::color_scheme::system_color("canvas",scheme)}
            }else {self.canvas_background};
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
            // Overflow bounds are expressed before the container's own scroll
            // translation. A changed box or writing direction can therefore
            // clamp existing offsets after layout, with at most one correction
            // layout before publishing commands and hit geometry.
            let mut corrected = false;
            let mut sampled_timelines=false;
            let mut timeline_corrected=false;
            let (mut list, mut geometry) = loop {
                let mut geometry = layout::LayoutGeometry::default();
                geometry.collect_rendered_text = self.rendered_text_requested;
                let list = {
            let node_images = NodeImages {
                color_schemes:self.media_environment().color_schemes,
                svg_fragment:self.svg_fragment.as_deref(), svg_image_viewport:self.svg_image_viewport.as_deref().copied(),
                paint_definitions: &self.paint_definitions,
                paint_registration_revision:self.rules_generation,
                bitmaps: &self.node_bitmaps, embedded:&self.embedded_bitmaps,
                objects: &self.object_representations,
                fallback: images,
            };
                    layout::display_list_with_snapshot_mode_and_timelines(
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
                    canvas_background,
                    None,
                    self.timeline_transform_values.as_deref(),
                )?
                };
                let mut changed = false;
                if !corrected {for scroll in &mut self.scrolls {
                    let bounds = geometry.scroll_extents.iter().find(|bounds| bounds.node == scroll.node);
                    let (x, y) = bounds.map_or((0.0, 0.0), |bounds| (
                        scroll.x.clamp(bounds.min_x, bounds.max_x),
                        scroll.y.clamp(bounds.min_y, bounds.max_y),
                    ));
                    if (scroll.x,scroll.y)!=(x,y) {
                        if !self.normalized_scroll_events.contains(&scroll.node) {
                            self.normalized_scroll_events.try_reserve(1).map_err(|_|LayoutError::CommandLimit)?;
                            self.normalized_scroll_events.push(scroll.node);
                        }
                        changed=true;
                    }
                    scroll.x = x;
                    scroll.y = y;
                }}
                if changed {
                    self.paint_revision = self.paint_revision.wrapping_add(1);
                    corrected=true;
                }
                // Normalize actual offsets before the single post-layout
                // stale-timeline update. A changed transform and scroll clamp
                // share the same bounded correction layout. Forced CSSOM
                // layouts preserve the previously sampled timeline value.
                if !sampled_timelines&&!self.forced_layout {
                    sampled_timelines=true;
                    timeline_corrected=self.sample_transform_timeline_values(&geometry,text)?;
                    changed|=timeline_corrected;
                }
                if !changed {break (list,geometry);}
                self.layout_cache.clear();
                self.layout_cache.begin_frame();
            };
            let mut requests = Vec::new();
            apply_paint_worklet_cache(&mut list.0, &self.paint_worklet_cache, &mut requests);
            self.paint_worklet_cache.retain(|(request,_)|requests.contains(request));
            geometry.rendered_text_boxes.sort_unstable_by_key(|value| (value.node.key(), value.order));
            geometry.rendered_table_members.sort_unstable_by_key(|value| value.node.key());
            // A correction can itself change a timeline range/scope. Keep
            // that operation stale until the next rendering opportunity,
            // without sampling it a second time in this frame.
            self.timeline_layout_stale=geometry.transform_timeline_values.is_some()&&(self.forced_layout||timeline_corrected);
            self.frame_id += 1;
            self.cached = Some(CachedFrame {
                document_version: version,
                width,
                height,
                // Lazy resolvers may admit/decode an image while constructing
                // this frame. Its completed commands own that generation.
                image_generation:images.map(ImageResolver::generation),
                list,
                geometry,
            });
            // Consume source changes while their mutation journal still
            // exists, even when this layout reused the stylesheet index.
            // Otherwise a later source query cannot distinguish an unrelated
            // style edit from an unobserved replacement of a loaded sheet.
            self.reclaim_stale_stylesheet_sources();
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

    #[test]
    fn specification_transform_timeline_uses_completed_owner_geometry_and_render_phase() {
        let document=crate::html::parse("<style>#port{width:100px;height:100px;overflow:auto;scroll-timeline:--owner block}#content{height:300px}#box{width:20px;height:10px;transform-origin:0 0;transform:transform-interpolate(--owner,0%:translateX(0px),100%:translateX(100px))}</style><div id=port><div id=content><div id=box></div></div></div>",128).unwrap();
        let port=crate::selector::query_selector(&document,document.root(),"#port").unwrap().unwrap();
        let node=crate::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(300,200,&NoText).unwrap();
        let initial=session.bounding_client_rect(node).unwrap();
        let initial_style=session.computed_style(node).unwrap();
        let initial_used=session.used_transform_timeline_source(node,None,&initial_style).unwrap();
        assert!(session.set_scroll_offset(port,0.0,100.0).unwrap());
        session.with_forced_layout(|session|session.display_list(300,200,&NoText).map(|_|())).unwrap();
        let forced_style=session.computed_style(node).unwrap();
        assert_eq!(session.used_transform_timeline_source(node,None,&forced_style).unwrap(),initial_used,"a forced query cannot advance a stale timeline");
        session.display_list(300,200,&NoText).unwrap();
        let rect=session.bounding_client_rect(node).unwrap();
        assert_eq!(rect.x-initial.x,50.0,"paint and hit geometry use the actual owner scroll range");
        let style=session.computed_style(node).unwrap();
        let used=session.used_transform_timeline_source(node,None,&style).unwrap();
        assert_ne!(used,initial_used);
        assert!(session.cached.as_ref().unwrap().geometry.transform_timeline_values.is_some());
        session.document_mut().set_attribute(node,"style","transform:none").unwrap();
        session.display_list(300,200,&NoText).unwrap();
        let style=session.computed_style(node).unwrap();
        assert!(session.used_transform_timeline_source(node,None,&style).is_none());
        assert!(session.cached.as_ref().unwrap().geometry.transform_timeline_values.is_none());
        assert!(session.timeline_transform_values.is_none());
    }

    #[test]
    fn specification_transform_timeline_absolute_length_and_inactive_progress_use_one_authority() {
        let document=crate::html::parse("<style>#port{width:100px;height:100px;overflow:auto;scroll-timeline:--owner block}#content{height:300px}#box{width:20px;height:10px;transform-origin:0 0;transform:transform-interpolate(--missing,0px:translateX(20px),200px:translateX(120px))}</style><div id=port><div id=content><div id=box></div></div></div>",128).unwrap();
        let port=crate::selector::query_selector(&document,document.root(),"#port").unwrap().unwrap();
        let node=crate::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        // No existing rendering opportunity: initially stale and missing
        // sources both mean proportional progress zero, even for length stops.
        session.with_forced_layout(|session|session.display_list(300,200,&NoText).map(|_|())).unwrap();
        let initially_stale=session.bounding_client_rect(node).unwrap();
        session.display_list(300,200,&NoText).unwrap();
        assert_eq!(session.bounding_client_rect(node).unwrap().x,initially_stale.x,"missing timeline selects the same zero-progress endpoint");
        session.document_mut().set_attribute(node,"style","transform:transform-interpolate(--owner,0px:translateX(20px),200px:translateX(120px))").unwrap();
        session.display_list(300,200,&NoText).unwrap();
        assert!(session.set_scroll_offset(port,0.0,100.0).unwrap());
        session.display_list(300,200,&NoText).unwrap();
        assert_eq!(session.bounding_client_rect(node).unwrap().x-initially_stale.x,50.0,"absolute progress uses actual 100px owner scroll coordinate and map stop range");
        session.document_mut().set_attribute(port,"style","overflow:visible").unwrap();
        session.display_list(300,200,&NoText).unwrap();
        assert_eq!(session.bounding_client_rect(node).unwrap().x,initially_stale.x,"a named timeline without an active scrollport returns proportional zero");
    }

    #[test]
    fn specification_transform_timeline_absolute_time_is_invalid_at_computed_values() {
        let source="transform-interpolate(scroll(),0s:translateX(0px),1s:translateX(100px))";
        assert!(css::supports_property_value("transform",source),"absolute time stops are part of the map syntax");
        let document=crate::html::parse("<style>#box{transform:transform-interpolate(scroll(),0s:translateX(0px),1s:translateX(100px))}</style><div id=box></div>",64).unwrap();
        let node=crate::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let style=session.computed_style(node).unwrap();
        assert!(!style.has_transform(),"a time range cannot establish a transform on a length-based CSS scroll timeline");
        session.display_list(200,100,&NoText).unwrap();
        assert!(session.cached.as_ref().unwrap().geometry.transform_timeline_values.is_none());
    }

    #[test]
    fn specification_deferred_transform_map_drives_real_paint_and_hit_geometry() {
        let document=crate::html::parse("<style>#box{width:20px;height:10px;background:red;transform-origin:0 0;transform:transform-interpolate(50%,0%:translateX(0%),100%:scale(2))}</style><div id=box></div>",64).unwrap();
        let node=crate::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let style=session.computed_style(node).unwrap();
        assert!(style.has_transform(),"a valid deferred source still establishes a transform");
        let commands=&session.display_list(200,100,&NoText).unwrap().0;
        assert!(commands.iter().any(|command|matches!(command,crate::paint::Command::PushTransform(matrix) if matrix.a==1.5&&matrix.d==1.5)));
        let rect=session.bounding_client_rect(node).unwrap();
        assert_eq!((rect.width,rect.height),(30.0,15.0),"hit geometry uses the same actual reference-box matrix as paint");
        session.document_mut().set_attribute(node,"style","transform:none").unwrap();
        session.display_list(200,100,&NoText).unwrap();
        let rect=session.bounding_client_rect(node).unwrap();
        assert_eq!((rect.width,rect.height),(20.0,10.0),"source removal clears transform paint and geometry");
    }

    #[test]
    fn specification_effect_underlying_snapshot_selects_ordered_lineage_and_requested_pseudos() {
        let document=crate::html::parse("<style>#parent{font-size:20px;color:red}#leaf{width:2em}#leaf::before{content:'x';color:blue}</style><section id=parent><div id=leaf></div><aside id=other></aside></section>",64).unwrap();
        let leaf=crate::selector::query_selector(&document,document.root(),"#leaf").unwrap().unwrap();
        let other=crate::selector::query_selector(&document,document.root(),"#other").unwrap().unwrap();
        let parent=crate::selector::query_selector(&document,document.root(),"#parent").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let full=session.transition_snapshot().unwrap();
        let selected=session.effect_underlying_snapshot_with_text(&[(leaf,Some(css::PseudoElement::Before)),(leaf,None),(parent,None)],None).unwrap();
        assert!(selected.nodes.iter().all(|input|input.node!=other));
        let parent_index=selected.nodes.iter().position(|input|input.node==parent&&input.pseudo.is_none()).unwrap();
        let leaf_index=selected.nodes.iter().position(|input|input.node==leaf&&input.pseudo.is_none()).unwrap();
        assert!(parent_index<leaf_index);
        assert_eq!(selected.nodes.iter().filter(|input|input.node==leaf&&input.pseudo.is_none()).count(),1);
        assert_eq!(selected.nodes.iter().filter(|input|input.pseudo.is_some()).count(),1);
        for input in &selected.nodes {
            let expected=full.nodes.iter().find(|expected|expected.node==input.node&&expected.pseudo==input.pseudo).unwrap();
            assert_eq!(input.parent,expected.parent);
            assert_eq!(input.style.font_size,expected.style.font_size);
            assert_eq!(input.style.width,expected.style.width);
            assert_eq!(input.style.color,expected.style.color);
            assert_eq!(input.was_rendered(),expected.was_rendered());
        }
        session.document_mut().set_attribute(parent,"style","font-size:30px").unwrap();
        let changed=session.effect_underlying_snapshot_with_text(&[(leaf,None)],None).unwrap();
        let changed=changed.nodes.iter().find(|input|input.node==leaf).unwrap();
        assert_eq!(changed.style.font_size,30.0);
        assert_eq!(changed.style.width,Some(60.0));
    }

    struct NoText;
    #[test]
    fn specification_render_snapshot_budget_rejection_is_stable_and_retries_changed_inputs() {
        // A wide connected tree exceeds the existing provenance byte budget;
        // no depth, display-command, or arbitrary node-count limit is changed.
        let source = alloc::format!("<section id=large>{}</section>","<div></div>".repeat(20_000));
        let document = crate::html::parse(&source,24_000).unwrap();
        let large = crate::selector::query_selector(&document,document.root(),"#large").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        assert!(matches!(session.transition_snapshot(),Err(LayoutError::CommandLimit)));
        let selective=session.effect_underlying_snapshot_with_text(&[(large,None),(large,None),(large,Some(css::PseudoElement::Before))],None).unwrap();
        assert!(selective.nodes.len()<8,"one target does not capture the 20,000 unrelated descendant styles");
        assert_eq!(selective.nodes.iter().filter(|input|input.node==large&&input.pseudo.is_none()).count(),1);
        let absent=selective.nodes.iter().find(|input|input.node==large&&input.pseudo==Some(css::PseudoElement::Before)).unwrap();
        assert!(!absent.was_rendered(),"requested underlying pseudo style does not fabricate a generated box");
        let rejected = session.rejected_transition_snapshot.as_deref().copied().expect("deterministic provenance budget was reached");
        for _ in 0..4 {
            assert!(matches!(session.transition_snapshot(),Err(LayoutError::CommandLimit)));
            assert!(session.rejected_transition_snapshot.as_deref()==Some(&rejected));
        }
        session.document_mut().remove(large).unwrap();
        let recovered = session.transition_snapshot().unwrap();
        assert!(recovered.nodes.len()<8);
        assert!(session.rejected_transition_snapshot.is_none());
        let mut fresh = RenderSession::new(session.document().clone_document(true).unwrap());
        let expected = fresh.transition_snapshot().unwrap();
        assert_eq!(recovered.nodes.len(),expected.nodes.len());
        assert!(recovered.nodes.iter().zip(&expected.nodes).all(|(actual,expected)|actual.pseudo==expected.pseudo && actual.was_rendered()==expected.was_rendered() && actual.style.display==expected.style.display && actual.style.font_size==expected.style.font_size && actual.style.color==expected.style.color));
    }


    #[test]
    fn specification_used_color_scheme_meta_preferences_branches_and_inheritance() {
        let document=crate::html::parse("<!doctype html><meta id=scheme name=color-scheme content='light dark'><style>#parent{color:light-dark(green,red)}#child{color-scheme:light;background-color:currentColor}#palette{color:CanvasText;background-color:Canvas}</style><div id=parent><div id=child></div></div><div id=palette></div>",128).unwrap();
        let meta=crate::selector::query_selector(&document,document.root(),"#scheme").unwrap().unwrap();
        let child=crate::selector::query_selector(&document,document.root(),"#child").unwrap().unwrap();
        let palette=crate::selector::query_selector(&document,document.root(),"#palette").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.set_color_scheme_preference(css::ColorSchemePreference::Dark).unwrap();
        assert_eq!(session.computed_style(child).unwrap().background,Rgba{r:255,g:0,b:0,a:255},"inherited computed color freezes parent's branch before child scheme");
        let dark=session.computed_style(palette).unwrap();
        assert_eq!(dark.color,css::color_scheme::system_color("canvastext",css::UsedColorScheme::Dark).unwrap());
        assert_eq!(dark.background,css::color_scheme::system_color("canvas",css::UsedColorScheme::Dark).unwrap());
        let epoch=session.transition_input_epoch().unwrap();
        session.set_color_scheme_preference(css::ColorSchemePreference::Dark).unwrap();
        assert!(session.transition_input_epoch().unwrap()==epoch,"unchanged scheme retains canonical identity");
        session.document_mut().set_attribute(meta,"content","only light").unwrap();
        let light=session.computed_style(palette).unwrap();
        assert_eq!(light.color,css::color_scheme::system_color("canvastext",css::UsedColorScheme::Light).unwrap());
        assert!(css::media_query_matches("(prefers-color-scheme: light)",session.media_environment()));
        assert!(session.transition_input_epoch().unwrap()!=epoch,"metadata mutates the same style/effect input identity");
    }

    #[test]
    fn specification_render_environment_reuses_warm_layout_and_observes_real_changes() {
        let document = crate::html::parse("<style>@media(min-width:150px){#target{width:60px}}#target{height:20px}</style><div id=target style=background:lime></div>",64).unwrap();
        let target = crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        let environment = css::MediaEnvironment {width:100.0,height:80.0,..css::MediaEnvironment::default()};
        session.set_media_environment(environment).unwrap();
        let first = session.display_list(100,80,&NoText).unwrap().clone();
        let revision = session.paint_revision();
        let frame = session.frame_id();
        for _ in 0..8 {
            session.set_media_environment(environment).unwrap();
            assert!(session.cached.is_some(),"an unchanged provider environment retains the completed frame");
            assert_eq!(session.display_list(100,80,&NoText).unwrap(),&first);
            assert_eq!(session.paint_revision(),revision);
            assert_eq!(session.frame_id(),frame);
        }
        let changed = css::MediaEnvironment {width:200.0,..environment};
        session.set_media_environment(changed).unwrap();
        assert!(session.cached.is_none());
        let resized = session.display_list(200,80,&NoText).unwrap().clone();
        assert_eq!(session.layout_rect(target).unwrap().width,60.0);
        let mut fresh = RenderSession::new(session.document().clone_document(true).unwrap());
        fresh.set_media_environment(changed).unwrap();
        assert_eq!(resized,fresh.display_list(200,80,&NoText).unwrap().clone());
        session.document_mut().set_attribute(target,"style","width:30px;background:red").unwrap();
        session.set_media_environment(changed).unwrap();
        let mutated = session.display_list(200,80,&NoText).unwrap().clone();
        assert_eq!(session.layout_rect(target).unwrap().width,30.0);
        let mut fresh = RenderSession::new(session.document().clone_document(true).unwrap());
        fresh.set_media_environment(changed).unwrap();
        assert_eq!(mutated,fresh.display_list(200,80,&NoText).unwrap().clone());
    }

    #[test]
    fn specification_generic_xml_boxes_namespace_styles_geometry_and_retained_invalidation() {
        let document=crate::xml::parse(r#"<root xmlns="urn:boxes"><style xmlns="http://www.w3.org/1999/xhtml">@namespace u "urn:boxes";u|box{display:inline-block;width:20px;height:30px;vertical-align:top;background:lime}u|br{display:inline-block;width:10px;height:12px;vertical-align:top;background:red}</style><box id="target"/><br id="literal"/></root>"#,64).unwrap();
        let target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let literal=crate::selector::query_selector(&document,document.root(),"#literal").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let first=session.display_list(100,80,&NoText).unwrap().clone();
        let rect=session.layout_rect(target).unwrap();
        assert_eq!((rect.width,rect.height),(20.0,30.0));
        let other=session.layout_rect(literal).unwrap();
        assert_eq!((other.x,other.width,other.height),(20.0,10.0,12.0),"an XML element named br has its CSS box and does not become an HTML line break");
        assert!(first.0.iter().any(|command|matches!(command,crate::paint::Command::FillRect{rect,color} if rect.width==20.0 && rect.height==30.0 && color.r==0 && color.g==255 && color.b==0)));
        session.document_mut().set_attribute(target,"style","width:40px").unwrap();
        let changed=session.display_list(100,80,&NoText).unwrap().clone();
        assert_eq!(session.layout_rect(target).unwrap().width,40.0);
        assert_eq!(session.layout_rect(literal).unwrap().x,40.0);
        let mut fresh=RenderSession::new(session.document().clone_document(true).unwrap());
        assert_eq!(changed,fresh.display_list(100,80,&NoText).unwrap().clone(),"generic XML uses the same invalidation and fresh layout path as HTML");
        assert_eq!(changed,session.display_list(100,80,&NoText).unwrap().clone());
    }

    #[test]
    fn specification_rendered_svg_text_boxes_belong_to_paint_not_geometry_or_use_instances() {
        for effect in ["", "filter='url(#f)'"] {
            let markup=alloc::format!("<!doctype html><div id=target><svg width=100 height=80><defs><filter id=f><feColorMatrix/></filter></defs><text id=source {effect}>a<tspan>b</tspan>c</text><use href='#source' x=20/></svg></div>");
            let document=crate::html::parse(&markup,128).unwrap();
            let target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
            let mut session=RenderSession::new(document);
            session.request_rendered_text_boxes();
            for _ in 0..2 {
                session.display_list(100,80,&NoText).unwrap();
                assert_eq!(crate::rendered_text::get(&mut session,target).unwrap(),"abc",
                    "actual DOM text boxes are collected once; geometry probes and use instances create no duplicate DOM text");
                assert_eq!(session.rendered_text_boxes().unwrap().len(),3);
            }
            let document=session.document().clone_document(true).unwrap();
            let fresh_target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
            let mut fresh=RenderSession::new(document);
            fresh.request_rendered_text_boxes();fresh.display_list(100,80,&NoText).unwrap();
            assert_eq!(crate::rendered_text::get(&mut fresh,fresh_target).unwrap(),"abc");
        }
    }

    #[test]
    fn specification_rendered_text_actual_box_ownership_and_raw_unrendered_descendants() {
        let document=crate::html::parse("<!doctype html><svg id=svg></svg><div id=target style='text-transform:uppercase'> a  b <span style='display:none'> c </span></div><div id=empty style='width:0;height:0'></div>",64).unwrap();
        let root=document.root();
        let svg=crate::selector::query_selector(&document,root,"#svg").unwrap().unwrap();
        let target=crate::selector::query_selector(&document,root,"#target").unwrap().unwrap();
        let empty=crate::selector::query_selector(&document,root,"#empty").unwrap().unwrap();
        let body=crate::selector::query_selector(&document,root,"body").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.document_mut().append(svg,target).unwrap();
        session.request_rendered_text_boxes();
        session.display_list(100,80,&NoText).unwrap();
        assert!(session.layout_rect(target).is_none(),"an HTML child outside SVG foreignObject has no associated layout box");
        assert_eq!(crate::rendered_text::get(&mut session,target).unwrap()," a  b  c ","unrendered getter returns actual descendant text without whitespace, visibility or case projection");
        assert!(session.layout_rect(empty).is_some(),"a degenerate CSS box still counts as rendered");
        assert_eq!(crate::rendered_text::get(&mut session,empty).unwrap(),"");
        session.document_mut().append(body,target).unwrap();
        session.document_mut().set_attribute(target,"style","display:none;text-transform:uppercase").unwrap();
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(crate::rendered_text::get(&mut session,target).unwrap()," a  b  c ");
        let document=session.document().clone_document(true).unwrap();
        let fresh_target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let mut fresh=RenderSession::new(document);
        fresh.request_rendered_text_boxes();fresh.display_list(100,80,&NoText).unwrap();
        assert_eq!(crate::rendered_text::get(&mut fresh,fresh_target).unwrap(),crate::rendered_text::get(&mut session,target).unwrap(),"retained and fresh box ownership agree after reparenting");
    }

    #[test]
    fn specification_window_rootless_document_replaces_retained_geometry_and_restores_real_root() {
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}#target{width:40px;height:300px;background:green}</style><div id=target></div>",64).unwrap();
        let mut session=RenderSession::new(document);let root=session.document().root();
        let html=crate::selector::document_element(session.document()).unwrap();
        let target=crate::selector::query_selector(session.document(),root,"#target").unwrap().unwrap();
        session.request_rendered_text_boxes();
        session.display_list(100,80,&NoText).unwrap();assert!(session.layout_rect(target).is_some());
        assert!(session.set_scroll_offset(root,0.0,200.0).unwrap());session.display_list(100,80,&NoText).unwrap();
        session.document_mut().remove(html).unwrap();
        let blank=session.display_list(100,80,&NoText).unwrap().clone();
        assert_eq!(blank.0,vec![crate::paint::Command::FillRect{rect:crate::paint::Rect{x:0.0,y:0.0,width:100.0,height:80.0},color:layout::DEFAULT_CANVAS_BACKGROUND}],"rootless canvas retains only its configured viewport background");
        assert_eq!(session.scroll_offset(root),(0.0,0.0));assert!(session.layout_rect(target).is_none());
        let frame=session.cached.as_ref().unwrap();assert!(frame.geometry.hits.is_empty());assert!(frame.geometry.hit_fragments.is_empty());assert!(frame.geometry.scroll_extents.is_empty());
        assert!(frame.geometry.collect_rendered_text);assert_eq!(frame.geometry.scroll_ports.len(),1);assert_eq!(frame.geometry.scroll_ports[0].node,root);
        assert_eq!((session.layout_cache_stats().entries,session.layout_cache_stats().bytes),(0,0));
        assert_eq!(blank,layout::display_list(session.document(),100,80,&NoText).unwrap());
        session.document_mut().append(root,html).unwrap();
        let restored=session.display_list(100,80,&NoText).unwrap().clone();assert!(session.layout_rect(target).is_some());
        assert_eq!(restored,layout::display_list(session.document(),100,80,&NoText).unwrap());
        let mut empty=RenderSession::new(Document::new(8));
        assert_eq!(empty.display_list(100,80,&NoText).unwrap().0,vec![crate::paint::Command::FillRect{
            rect:crate::paint::Rect{x:0.0,y:0.0,width:100.0,height:80.0},color:layout::DEFAULT_CANVAS_BACKGROUND}],
            "a rootless document preserves the configured host canvas");
        empty.set_canvas_background(None);assert!(empty.display_list(100,80,&NoText).unwrap().0.is_empty());

        let unsupported=crate::xml::parse("<math xmlns=\"http://www.w3.org/1998/Math/MathML\"/>",8).unwrap();
        assert_eq!(layout::display_list(&unsupported,100,80,&NoText),Err(LayoutError::InvalidTree),"a present unsupported root is not mistaken for a rootless document");
    }

    #[test]
    fn specification_read_style_cache_distinguishes_absent_backend_and_actual_generation() {
        struct Metrics { generation: Cell<u64>, ch: Cell<f32> }
        impl TextShaper for Metrics {
            fn shape(&self, _: &str, _: f32) -> Result<ShapedRun, ()> {
                Ok(ShapedRun { glyphs: Arc::from([]), width: 0.0 })
            }
            fn ascent(&self, _: f32) -> f32 { 0.0 }
            fn line_height(&self, _: f32) -> f32 { 0.0 }
            fn generation(&self) -> u64 { self.generation.get() }
            fn font_relative_metrics_styled(&self, size: f32, _: &crate::paint::FontSpec) -> crate::paint::FontRelativeMetrics {
                crate::paint::FontRelativeMetrics { ex: size * 0.25, ch: size * self.ch.get() }
            }
        }
        let document=crate::html::parse("<div id=target style='font-size:10px;width:2ch'></div>",32).unwrap();
        let target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let text=Metrics {generation:Cell::new(1),ch:Cell::new(0.75)};
        assert_eq!(session.computed_style(target).unwrap().width,Some(10.0));
        assert_eq!(session.computed_style_with_text(target,Some(&text)).unwrap().width,Some(15.0));
        let before=session.read_style_cache_stats();
        assert_eq!(session.computed_style_with_text(target,Some(&text)).unwrap().width,Some(15.0));
        let after=session.read_style_cache_stats();
        assert_eq!(before.computed_styles,after.computed_styles);
        assert!(after.cache_hits>before.cache_hits);
        text.ch.set(0.9);text.generation.set(2);
        assert_eq!(session.computed_style_with_text(target,Some(&text)).unwrap().width,Some(18.0));
        assert_eq!(session.computed_style(target).unwrap().width,Some(10.0));
    }

    #[test]
    fn cssom_source_overlay_preserves_author_nodes_and_old_sheet_transactions() {
        let mut document = crate::html::parse("<style>.box {width:7px}</style><div class=box></div>", 32).unwrap();
        let owner = crate::selector::query_selector(&document, document.root(), "style").unwrap().unwrap();
        let target = crate::selector::query_selector(&document, document.root(), ".box").unwrap().unwrap();
        let first = document.first_child(owner).unwrap().unwrap();
        let extra = document.create(NodeKind::Text(".box {height:3px}".into())).unwrap();
        document.append(owner, extra).unwrap();
        document.clear_mutations();
        let mut session = RenderSession::new(document);
        let lease = session.stylesheet_root_lease(owner).unwrap();
        let version = session.document().version();
        session.replace_cssom_stylesheet_text(owner, &lease, ".box {width:19px}", None).unwrap();
        assert_eq!(session.document().version(), version);
        assert!(session.document().mutations().is_empty());
        assert_eq!(session.document().first_child(owner).unwrap(), Some(first));
        assert_eq!(session.document().next_sibling(first).unwrap(), Some(extra));
        assert!(matches!(session.document().kind(first), Ok(NodeKind::Text(text)) if text == ".box {width:7px}"));
        assert_eq!(session.computed_style(target).unwrap().width, Some(19.0));
        assert!(session.replace_cssom_stylesheet_text(owner, &lease, &"x".repeat(1024 * 1024 + 1), None).is_err());
        assert_eq!(session.computed_style(target).unwrap().width, Some(19.0));
        session.document_mut().replace_data(first, ".box {width:31px}").unwrap();
        session.reclaim_stale_stylesheet_sources();
        let fresh = session.stylesheet_root_lease(owner).unwrap();
        assert!(lease.is_detached());
        assert!(!Rc::ptr_eq(&lease, &fresh));
        assert_eq!(lease.with_text(|text| String::from(text)), ".box {width:19px}");
        session.replace_cssom_stylesheet_text(owner, &lease, ".box {width:41px}", None).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(31.0));
        session.replace_cssom_stylesheet_text(owner, &fresh, ".box {width:43px}", None).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(43.0));
        let weak = Rc::downgrade(&fresh);
        drop(fresh);
        assert!(weak.upgrade().is_none(), "effective text must not retain a native wrapper lease");
        assert_eq!(session.stylesheet_root_lease(owner).unwrap().with_text(|text| String::from(text)), ".box {width:43px}", "CSSOM effective bytes survive wrapper GC");
    }

    #[test]
    fn specification_import_disabled_lease_invalidates_paint_preserves_source_and_replays_detached_flags() {
        let text="@import 'child.css';body{margin:0}div{height:7px;background:blue}";
        let fixture=alloc::format!("<style>{text}</style><div id=box></div>");
        let document=crate::html::parse(&fixture,64).unwrap();
        let owner=crate::selector::query_selector(&document,document.root(),"style").unwrap().unwrap();
        let target=crate::selector::query_selector(&document,document.root(),"#box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.set_stylesheet_source(owner,Some(css::StylesheetSource{disabled:false,url:Arc::from("https://style.test/root.css"),text:Arc::from(text),
            imports:vec![css::LoadedImport{rule:css::imports(text).unwrap().remove(0),source:Some(Box::new(css::StylesheetSource{disabled:false,
                url:Arc::from("https://style.test/child.css"),text:Arc::from("div{width:23px}"),imports:Vec::new()}))}]})).unwrap();
        let lease=session.stylesheet_import_lease(owner,vec![0]).unwrap();
        let before=session.display_list(100,80,&NoText).unwrap().clone();
        assert_eq!(session.layout_rect(target).unwrap().width,23.0);
        let version=session.document().version();
        session.set_stylesheet_import_disabled(owner,&lease,true).unwrap();
        assert_eq!(session.document().version(),version);
        let disabled=session.display_list(100,80,&NoText).unwrap().clone();
        assert_eq!(session.layout_rect(target).unwrap().width,100.0);
        let graph=session.stylesheet_source(owner).unwrap().clone();
        let document=crate::html::parse(&fixture,64).unwrap();
        let fresh_owner=crate::selector::query_selector(&document,document.root(),"style").unwrap().unwrap();
        let mut fresh=RenderSession::new(document);fresh.set_stylesheet_source(fresh_owner,Some(graph)).unwrap();
        assert_eq!(disabled,fresh.display_list(100,80,&NoText).unwrap().clone());
        assert_eq!(disabled,session.display_list(100,80,&NoText).unwrap().clone());
        session.set_stylesheet_import_disabled(owner,&lease,false).unwrap();
        assert_eq!(before,session.display_list(100,80,&NoText).unwrap().clone());
        let frame=session.frame_id;session.set_stylesheet_import_disabled(owner,&lease,false).unwrap();assert_eq!(session.frame_id,frame);
        session.document_mut().remove(owner).unwrap();session.reclaim_stale_stylesheet_sources();
        assert!(lease.is_detached());
        assert_eq!(lease.with_detached_source_mut(|source|{let source=source.unwrap();source.disabled=true;source.disabled}),Some(true));
        assert_eq!(lease.with_detached_source(|source|source.unwrap().disabled),Some(true));
        assert!(session.computed_style(target).unwrap().width.is_none());
    }

    #[test]
    fn specification_stylesheet_import_completions_follow_occurrences_and_preserve_edited_roots() {
        let text="@import 'same.css';@import 'same.css';.box{height:3px}";
        let document=crate::html::parse(&alloc::format!("<style>{text}</style><div class=box></div>"),64).unwrap();
        let owner=crate::selector::query_selector(&document,document.root(),"style").unwrap().unwrap();
        let target=crate::selector::query_selector(&document,document.root(),".box").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.set_stylesheet_source(owner,Some(css::StylesheetSource{disabled:false, url:Arc::from("https://style.test/root.css"),text:Arc::from(text),
            imports:css::imports(text).unwrap().into_iter().map(|rule|css::LoadedImport{rule,source:None}).collect()})).unwrap();
        let root=session.stylesheet_root_lease(owner).unwrap();
        let first=session.stylesheet_import_rule_lease(owner,vec![0]).unwrap();
        let second=session.stylesheet_import_rule_lease(owner,vec![1]).unwrap();
        assert!(Rc::ptr_eq(&first,&session.stylesheet_import_rule_lease(owner,vec![0]).unwrap()));
        let changed="@import 'fresh.css';@import 'same.css';.box{height:17px}";
        session.replace_cssom_stylesheet_text(owner,&root,changed,Some(&[None,Some(1)])).unwrap();
        assert!(session.stylesheet_import_rule(owner,&first).is_none(),"deleted occurrence cannot be identified by identical URL");
        let fresh=session.stylesheet_import_rule_lease(owner,vec![0]).unwrap();
        let leaf=|width|Box::new(css::StylesheetSource{disabled:false, url:Arc::from("https://style.test/same.css"),text:Arc::from(alloc::format!(".box{{width:{width}px}}")),imports:Vec::new()});
        assert_eq!(session.complete_stylesheet_import_occurrences(owner,vec![(first.clone(),leaf(99)),(second.clone(),leaf(23))]).unwrap(),1);
        assert_eq!(session.stylesheet_source(owner).unwrap().text.as_ref(),changed);
        assert!(session.stylesheet_source(owner).unwrap().imports[0].source.is_none());
        let style=session.computed_style(target).unwrap();assert_eq!(style.width,Some(23.));assert_eq!(style.height,Some(17.));
        assert_eq!(session.complete_stylesheet_import_occurrences(owner,vec![(fresh.clone(),leaf(11))]).unwrap(),1);
        session.replace_cssom_stylesheet_text(owner,&root,"@import 'fresh.css';.box{height:19px}",Some(&[Some(0)])).unwrap();
        assert!(session.stylesheet_import_rule(owner,&second).is_none());
        assert_eq!(session.complete_stylesheet_import_occurrences(owner,vec![(second,leaf(88))]).unwrap(),0);
        assert_eq!(session.computed_style(target).unwrap().width,Some(11.));
        assert!(!root.is_detached(),"completion retains canonical CSSOM root");
    }

    #[test]
    fn stylesheet_host_reinstall_moves_only_changed_owner_and_rejects_atomically() {
        let text = "@import 'child.css'; .box {height:3px}";
        let document = crate::html::parse(&alloc::format!("<style>{text}</style><div class=box></div>"), 32).unwrap();
        let owner = crate::selector::query_selector(&document, document.root(), "style").unwrap().unwrap();
        let graph = |width| css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/root.css"), text: Arc::from(text),
            imports: css::imports(text).unwrap().into_iter().map(|rule| css::LoadedImport { rule, source: Some(Box::new(css::StylesheetSource {disabled:false,
                url: Arc::from("https://example.test/child.css"), text: Arc::from(alloc::format!(".box {{width:{width}px}}")), imports: Vec::new(),
            })) }).collect(),
        };
        let mut session = RenderSession::new(document);
        session.set_stylesheet_source(owner, Some(graph(7))).unwrap();
        let lease = session.stylesheet_import_lease(owner, vec![0]).unwrap();
        let pointer = session.stylesheet_source(owner).unwrap().imports[0].source.as_deref().unwrap() as *const css::StylesheetSource;
        let epoch = lease.graph._epoch_token.get();
        session.set_stylesheet_source(owner, Some(graph(7))).unwrap();
        assert_eq!(lease.graph._epoch_token.get(), epoch, "identical host install is a no-op");
        assert!(!lease.is_detached());
        let mut rejected = graph(17); rejected.text = Arc::from(".other {}");
        assert!(session.set_stylesheet_source(owner, Some(rejected)).is_err());
        assert_eq!(lease.graph._epoch_token.get(), epoch);
        let target = crate::selector::query_selector(session.document(), session.document().root(), ".box").unwrap().unwrap();
        session.replace_cssom_stylesheet_text(owner, &lease.graph, "@import 'child.css'; .box {width:57px;height:3px}", None).unwrap();
        assert_eq!(session.computed_style(target).unwrap().width, Some(57.0));
        session.set_stylesheet_source(owner, Some(graph(19))).unwrap();
        assert!(lease.is_detached());
        assert_eq!(lease.with_detached_source(|source| source.unwrap() as *const css::StylesheetSource), Some(pointer));
        assert_eq!(lease.with_detached_source(|source| source.unwrap().text.clone()).unwrap().as_ref(), ".box {width:7px}");
        let fresh = session.stylesheet_import_lease(owner, vec![0]).unwrap();
        assert!(!Rc::ptr_eq(&lease.graph, &fresh.graph));
        assert_eq!(session.imported_stylesheet_text_ref(owner, &[0]), Some(".box {width:19px}"));
        assert_eq!(session.computed_style(target).unwrap().width, Some(19.0), "fresh graph must not render the old CSSOM effective text");
    }

    #[test]
    fn stylesheet_import_escrow_moves_rebased_graphs_across_aba_and_releases_weak_owners() {
        fn leaf(text: &str) -> css::StylesheetSource {
            css::StylesheetSource {disabled:false,  url: Arc::from("https://example.test/dup.css"), text: Arc::from(text), imports: Vec::new() }
        }
        let text = "@import 'dup.css'; @import 'dup.css'; .box {height:3px}";
        let document = crate::html::parse(&alloc::format!("<style>{text}</style><div class=box></div>"), 32).unwrap();
        let node = crate::selector::query_selector(&document, document.root(), "style").unwrap().unwrap();
        let box_node = crate::selector::query_selector(&document, document.root(), ".box").unwrap().unwrap();
        let mut second = leaf("@import 'nested.css'; .box {width:19px}");
        second.imports = vec![css::LoadedImport { rule: css::imports(&second.text).unwrap().remove(0), source: Some(alloc::boxed::Box::new(leaf(".nested {height:6px}"))) }];
        let root = css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/root.css"), text: Arc::from(text),
            imports: css::imports(text).unwrap().into_iter().zip([leaf(".box {width:17px}"), second]).map(|(rule, source)| css::LoadedImport { rule, source: Some(alloc::boxed::Box::new(source)) }).collect(),
        };
        let nested_pointer = root.imports[1].source.as_deref().unwrap().imports[0].source.as_deref().unwrap() as *const css::StylesheetSource;
        let mut session = RenderSession::new(document);
        session.set_stylesheet_source(node, Some(root)).unwrap();
        let second = session.stylesheet_import_lease(node, vec![1]).unwrap();
        let nested = session.stylesheet_import_lease(node, vec![1, 0]).unwrap();
        let weak = Rc::downgrade(&second.graph);
        let inserted = "@import 'dup.css'; @import 'dup.css'; @import 'dup.css'; .box {height:3px}";
        session.replace_stylesheet_text_with_import_map(node, inserted, Some(&[None, Some(0), Some(1)])).unwrap();
        let text_node = session.document().first_child(node).unwrap().unwrap();
        session.document_mut().replace_data(text_node, ".different {}").unwrap();
        session.document_mut().replace_data(text_node, inserted).unwrap();
        session.reclaim_stale_stylesheet_sources();
        assert!(session.stylesheet_source(node).is_none());
        assert_eq!(second.with_detached_source(|source| source.unwrap().text.clone()).unwrap().as_ref(), "@import 'nested.css'; .box {width:19px}");
        assert_eq!(nested.with_detached_source(|source| source.unwrap() as *const css::StylesheetSource).unwrap(), nested_pointer, "loaded descendants must move without cloning");
        let fresh = css::StylesheetSource {disabled:false,
            url: Arc::from("https://example.test/root.css"), text: Arc::from(inserted),
            imports: css::imports(inserted).unwrap().into_iter().enumerate().map(|(index, rule)| css::LoadedImport { rule, source: Some(alloc::boxed::Box::new(leaf(&alloc::format!(".box {{width:{}px}}", 40 + index)))) }).collect(),
        };
        session.set_stylesheet_source(node, Some(fresh)).unwrap();
        let fresh = session.stylesheet_import_lease(node, vec![2]).unwrap();
        assert!(!Rc::ptr_eq(&second.graph, &fresh.graph), "ABA source bytes must receive a fresh lease identity");
        second.with_detached_source_mut(|source| source.unwrap().text = Arc::from("@import 'nested.css'; .box {width:31px}")).unwrap();
        assert_eq!(session.computed_style(box_node).unwrap().width, Some(42.0));
        let before = session.document().first_child(node).unwrap();
        assert!(session.replace_stylesheet_text_with_import_map(node, inserted, Some(&[Some(0), Some(0), Some(2)])).is_err());
        assert_eq!(session.document().first_child(node).unwrap(), before);
        assert_eq!(fresh.path.borrow().as_deref(), Some(&[2][..]));
        drop(second); drop(nested);
        assert!(weak.upgrade().is_none(), "Session must not strongly retain expired imported wrappers");
        let extra = session.stylesheet_import_lease(node, vec![2]).unwrap();
        assert_eq!(session.stylesheet_graph_leases.len(), 1);
        assert!(Rc::ptr_eq(&extra.graph, &fresh.graph));
    }

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
    fn specification_image_button_bitmap_owner_used_dimensions_and_type_replay() {
        let document=crate::html::parse("<!doctype html><style>body{margin:0}input,img{display:block}</style><input id=button type=image src=actual.png width=30 height=20><img id=reference width=30 height=20>",64).unwrap();
        let button=crate::selector::get_element_by_id(&document,document.root(),"button").unwrap().unwrap();
        let reference=crate::selector::get_element_by_id(&document,document.root(),"reference").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.set_node_bitmap(button,None).unwrap();
        session.display_list(400,200,&NoText).unwrap();
        let rect=session.layout_rect(button).unwrap();assert_eq!((rect.width,rect.height),(30.0,20.0));
        let computed=session.computed_style(button).unwrap();
        assert_eq!(computed.appearance,crate::css::Appearance::None);
        assert_eq!(computed.used_border_widths(),[0.0;4],"image buttons do not acquire text-widget chrome");
        assert_eq!((computed.width,computed.height),(None,None),"13px primitive-control defaults do not override image dimensions");
        let image=Arc::new(ImageData{width:4,height:2,pixels:alloc::vec![0,128,0,255].repeat(8)});
        session.set_node_bitmap(button,Some(image.clone())).unwrap();session.set_node_bitmap(reference,Some(image.clone())).unwrap();
        let first=session.display_list(400,200,&NoText).unwrap().clone();
        assert_eq!(first.0.iter().filter(|command|matches!(command,crate::paint::Command::Image{image:published,..}if Arc::ptr_eq(published,&image))).count(),2);
        session.document_mut().set_attribute(button,"type","text").unwrap();session.document_mut().set_attribute(button,"style","display:none").unwrap();
        let replacement=Arc::new(ImageData{width:6,height:3,pixels:alloc::vec![0,0,255,255].repeat(18)});
        session.set_node_bitmap(button,Some(replacement.clone())).unwrap();
        assert!(!session.display_list(400,200,&NoText).unwrap().0.iter().any(|command|matches!(command,crate::paint::Command::Image{image:published,..}if Arc::ptr_eq(published,&replacement))));
        session.document_mut().set_attribute(button,"type","IMAGE").unwrap();session.document_mut().remove_attribute(button,"style").unwrap();
        session.document_mut().remove_attribute(button,"width").unwrap();session.document_mut().remove_attribute(button,"height").unwrap();
        let replay=session.display_list(400,200,&NoText).unwrap().clone();
        let rect=session.layout_rect(button).unwrap();assert_eq!((rect.width,rect.height),(6.0,3.0));
        assert!(replay.0.iter().any(|command|matches!(command,crate::paint::Command::Image{image:published,..}if Arc::ptr_eq(published,&replacement))));
        assert_eq!(replay,session.display_list(400,200,&NoText).unwrap().clone(),"warm state reuses the published request bitmap");
        session.document_mut().set_attribute(button,"style","width:72px;height:19px;padding:5px;border:3px solid").unwrap();
        session.display_list(400,200,&NoText).unwrap();let rect=session.layout_rect(button).unwrap();
        assert_eq!((rect.width,rect.height),(88.0,35.0),"author CSS still controls the real replaced content box and edges");
        session.document_mut().remove_attribute(button,"src").unwrap();
        assert!(!session.display_list(400,200,&NoText).unwrap().0.iter().any(|command|matches!(command,crate::paint::Command::Image{image:published,..}if Arc::ptr_eq(published,&replacement))),"missing src represents a button rather than its retained image");
        assert!(session.node_bitmaps.iter().any(|(node,published,_)|*node==button&&Arc::ptr_eq(published,&replacement)),"representation does not discard available request storage");
        session.document_mut().set_attribute(button,"src","actual.png").unwrap();
        assert!(session.display_list(400,200,&NoText).unwrap().0.iter().any(|command|matches!(command,crate::paint::Command::Image{image:published,..}if Arc::ptr_eq(published,&replacement))));
    }

    #[test]
    fn specification_committed_image_density_controls_resource_metrics_cold_and_warm() {
        use crate::{object::IntrinsicSize,responsive_images::ImageMetadata};
        let document=crate::html::parse("<!doctype html><style>body{margin:0}img{display:block}</style><img id=selected src=changed.svg>",64).unwrap();
        let node=crate::selector::get_element_by_id(&document,document.root(),"selected").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let image=Arc::new(ImageData{width:2,height:1,pixels:alloc::vec![0;2*4]});
        let natural=IntrinsicSize{width:Some(60.0),height:None,ratio:None};
        let metadata=ImageMetadata{intrinsic:Some(natural),density:2.0,source:Some(Arc::from("https://images.test/current.svg"))};
        session.set_node_bitmap_with_metadata(node,Some(image.clone()),metadata.clone()).unwrap();
        let first=session.display_list(400,200,&NoText).unwrap().clone();
        let rect=session.client_rects(node)[0];assert_eq!((rect.width,rect.height),(30.0,150.0),"density corrects actual intrinsic width, preserving the default height");
        let frame=session.frame_id();session.set_node_bitmap_with_metadata(node,Some(image.clone()),metadata.clone()).unwrap();assert_eq!(session.frame_id(),frame);
        assert_eq!(first,session.display_list(400,200,&NoText).unwrap().clone());
        session.set_node_bitmap_with_metadata(node,Some(image),ImageMetadata{density:1.0,..metadata}).unwrap();
        assert_ne!(session.frame_id(),frame);session.display_list(400,200,&NoText).unwrap();
        let rect=session.client_rects(node)[0];assert_eq!((rect.width,rect.height),(60.0,150.0));
        assert_eq!(natural.dimensions_at_density((300.0,150.0),2.0),(30.0,150.0));
        let huge=IntrinsicSize{width:Some(100.0),height:Some(50.0),ratio:Some(2.0)}.density_corrected(0.0).unwrap();
        assert!(huge.is_valid());assert_eq!(huge.width.unwrap()/huge.height.unwrap(),2.0,"zero-density rendering limit preserves the resource ratio");
    }

    #[test]
    fn specification_node_image_metadata_survives_resolverless_frames_and_same_bitmap_updates() {
        use crate::object::IntrinsicSize;
        let document=crate::html::parse("<!doctype html><style>body{margin:0}img{display:block}</style><img id=selected src=actual.svg>",64).unwrap();
        let node=crate::selector::get_element_by_id(&document,document.root(),"selected").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let image=Arc::new(ImageData{width:2,height:1,pixels:alloc::vec![0,128,0,255,0,128,0,255]});
        let natural=IntrinsicSize{width:None,height:None,ratio:Some(1.25)};
        session.set_node_bitmap_with_intrinsic(node,Some(image.clone()),Some(natural)).unwrap();
        let first=session.display_list(400,200,&NoText).unwrap().clone();
        let rect=first.0.iter().find_map(|command|match command {crate::paint::Command::Image{rect,image:painted} if Arc::ptr_eq(painted,&image)=>Some(*rect),_=>None}).unwrap();
        assert_eq!((rect.width,rect.height),(187.5,150.0),"natural metadata, not the two-pixel storage, sizes the real replaced box");
        let frame=session.frame_id();
        session.set_node_bitmap_with_intrinsic(node,Some(image.clone()),Some(natural)).unwrap();
        assert_eq!(session.frame_id(),frame,"same selected carrier and metadata reuse the frame");
        session.set_node_bitmap_with_intrinsic(node,Some(image.clone()),Some(IntrinsicSize{ratio:Some(2.0),..natural})).unwrap();
        assert_ne!(session.frame_id(),frame,"same pixels with changed resource dimensions invalidate layout");
        let changed=session.display_list(400,200,&NoText).unwrap();
        let rect=changed.0.iter().find_map(|command|match command {crate::paint::Command::Image{rect,..}=>Some(*rect),_=>None}).unwrap();
        assert_eq!((rect.width,rect.height),(300.0,150.0));
        assert!(first.0.iter().any(|command|matches!(command,crate::paint::Command::Image{rect,..} if rect.width==187.5)));
        session.document_mut().set_attribute(node,"style","width:100px;height:auto").unwrap();
        let changed=session.display_list(400,200,&NoText).unwrap();
        let rect=changed.0.iter().find_map(|command|match command {crate::paint::Command::Image{rect,..}=>Some(*rect),_=>None}).unwrap();
        assert_eq!((rect.width,rect.height),(100.0,50.0));
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
        let leaf = |url: &str, text: &str| css::StylesheetSource {disabled:false,
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
        let graph = css::StylesheetSource {disabled:false,
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
    fn stylesheet_source_generations_reclaim_loaded_overlays_on_external_aba() {
        let css_text = "@import 'old.css'; .box {width:3px;}";
        let document = crate::html::parse(&alloc::format!("<style>{css_text}</style><div class=box></div>"), 24).unwrap();
        let node = crate::selector::query_selector(&document, document.root(), "style").unwrap().unwrap();
        let box_node = crate::selector::query_selector(&document, document.root(), ".box").unwrap().unwrap();
        let text_node = document.first_child(node).unwrap().unwrap();
        let mut session = RenderSession::new(document);
        let token = session.stylesheet_change_token(node).unwrap();
        assert!(Rc::ptr_eq(&token, &session.stylesheet_change_token(node).unwrap()));
        let source_text: Arc<str> = Arc::from(css_text);
        let source_weak = Arc::downgrade(&source_text);
        let url: Arc<str> = Arc::from("https://source.test/final.css");
        session.set_stylesheet_source(node, Some(css::StylesheetSource {disabled:false,
            url: url.clone(), text: source_text.clone(),
            imports: vec![css::LoadedImport {
                rule: css::imports(css_text).unwrap().remove(0),
                source: Some(alloc::boxed::Box::new(css::StylesheetSource {disabled:false,
                    url: Arc::from("https://redirect.test/old-final.css"), text: Arc::from(".box {height:7px;}"), imports: Vec::new(),
                })),
            }],
        })).unwrap();
        let start = css_text.find("width").unwrap();
        session.stage_cssom_state(vec![css::RuleDeclarationOverride {
            owner: css::StylesheetIdentity::Dom(node), source_text: source_text.clone(), source_url: Some(url.clone()),
            declaration_offset: start, cssom_path: Arc::from([1usize, 0]),
            block: Rc::new(css::DeclarationBlock::parse("width:19px").unwrap()),
        }], vec![css::RuleTopologyOverride {
            owner: css::StylesheetIdentity::Dom(node), source_text: source_text.clone(), source_url: Some(url),
            boundaries: Arc::from([css::nesting::DeclarationBoundary { parent_start: css_text.find(".box").unwrap(), child_index: 0, range: start..start + "width:3px;".len() }]),
        }]).unwrap();
        drop(source_text);
        assert_eq!(session.computed_style(box_node).unwrap().width, Some(19.0));
        assert_eq!(session.computed_style(box_node).unwrap().height, Some(7.0));
        session.document_mut().replace_data(text_node, ".box {width:5px;}").unwrap();
        session.document_mut().replace_data(text_node, css_text).unwrap();
        assert!(session.stylesheet_source(node).is_none(), "pending ABA must not expose a stale captured final URL");
        let style = session.computed_style(box_node).unwrap();
        assert_eq!(style.width, Some(3.0));
        assert_eq!(style.height, None);
        assert_eq!(token.get(), 1);
        assert!(session.cssom_rule_overrides().is_empty());
        assert!(session.cssom_topology_overrides().is_empty());
        assert!(session.stylesheet_sources.is_empty());
        assert!(source_weak.upgrade().is_none(), "source Arc bytes must be released after replacement, not merely hidden from matching");
        session.replace_stylesheet_text(node, ".box {width:11px;}").unwrap();
        assert_eq!(token.get(), 1, "Session-owned DOM writes must not invalidate native CSSOM identities");
        assert_eq!(session.computed_style(box_node).unwrap().width, Some(11.0));
        session.document_mut().set_attribute(node, "media", "screen").unwrap();
        session.document_mut().set_attribute(node, "class", "changed").unwrap();
        session.reclaim_stale_stylesheet_sources();
        assert_eq!(token.get(), 1, "non-source attributes preserve stylesheet identity");
        session.document_mut().replace_data(text_node, ".box {width:13px;}").unwrap();
        session.reclaim_stale_stylesheet_sources();
        assert_eq!(token.get(), 2);
        assert_eq!(session.computed_style(box_node).unwrap().width, Some(13.0));
    }

    #[test]
    fn stylesheet_text_transactions_rebase_redirected_import_occurrences_and_roll_back() {
        fn leaf(url: &str, text: &str) -> css::StylesheetSource {
            css::StylesheetSource {disabled:false,  url: Arc::from(url), text: Arc::from(text), imports: Vec::new() }
        }
        let original = "@import './theme.css'; @import 'dup.css'; @import 'dup.css'; .box {height:4px;}";
        let document = crate::html::parse(&alloc::format!("<style>{original}</style><div class=box></div>"), 32).unwrap();
        let node = crate::selector::query_selector(&document, document.root(), "style").unwrap().unwrap();
        let box_node = crate::selector::query_selector(&document, document.root(), ".box").unwrap().unwrap();
        let text_node = document.first_child(node).unwrap().unwrap();
        let mut theme = leaf("https://cdn.test/assets/final-theme.css", "@import '../nested.css'; .box {color:red;}");
        theme.imports = vec![css::LoadedImport {
            rule: css::imports(&theme.text).unwrap().remove(0),
            source: Some(alloc::boxed::Box::new(leaf("https://redirect.test/nested-final.css", ".box {min-width:6px;}"))),
        }];
        let nested_text = theme.imports[0].source.as_ref().unwrap().text.clone();
        let first = leaf("https://redirect.test/first.css", ".box {width:7px;}");
        let second = leaf("https://redirect.test/second.css", ".box {width:9px;}");
        let first_text = first.text.clone();
        let second_text = second.text.clone();
        let rules = css::imports(original).unwrap();
        let root_url: Arc<str> = Arc::from("https://origin.test/dir/final-page.html");
        let graph = css::StylesheetSource {disabled:false,
            url: root_url.clone(), text: Arc::from(original),
            imports: rules.into_iter().zip([theme, first, second]).map(|(rule, source)| css::LoadedImport { rule, source: Some(alloc::boxed::Box::new(source)) }).collect(),
        };
        let mut session = RenderSession::new(document);
        session.set_stylesheet_source(node, Some(graph)).unwrap();
        let replacement = "@import 'dup.css'; @import 'theme.css'; @import 'dup.css'; .box {height:5px;}";
        session.stage_cssom_state(vec![css::RuleDeclarationOverride {
            owner: css::StylesheetIdentity::Dom(node), source_text: Arc::from(replacement), source_url: Some(root_url.clone()),
            declaration_offset: replacement.find("{height").unwrap() + 1, cssom_path: Arc::from([3usize]),
            block: alloc::rc::Rc::new(css::DeclarationBlock::parse("height:17px").unwrap()),
        }], Vec::new()).unwrap();
        session.replace_stylesheet_text(node, replacement).unwrap();
        assert_eq!(session.document().first_child(node).unwrap(), Some(text_node));
        let source = session.stylesheet_source(node).unwrap();
        assert!(Arc::ptr_eq(&source.url, &root_url));
        assert!(Arc::ptr_eq(&source.imports[0].source.as_ref().unwrap().text, &first_text));
        assert!(Arc::ptr_eq(&source.imports[2].source.as_ref().unwrap().text, &second_text));
        let theme = source.imports[1].source.as_ref().unwrap();
        assert_eq!(theme.url.as_ref(), "https://cdn.test/assets/final-theme.css");
        assert!(Arc::ptr_eq(&theme.imports[0].source.as_ref().unwrap().text, &nested_text));
        let style = session.computed_style(box_node).unwrap();
        assert_eq!(style.width, Some(9.0));
        assert_eq!(style.height, Some(17.0), "captured source URL must continue to match retained CSSOM declarations");
        assert_eq!(style.min_width, 6.0);
        let child_replacement = "@import '../nested.css' screen; .box {color:blue;}";
        session.replace_imported_stylesheet_text(node, &[1], child_replacement).unwrap();
        let source = session.stylesheet_source(node).unwrap();
        let theme = source.imports[1].source.as_ref().unwrap();
        assert!(Arc::ptr_eq(&theme.imports[0].source.as_ref().unwrap().text, &nested_text));
        assert_eq!(session.computed_style(box_node).unwrap().color, Rgba { r: 0, g: 0, b: 255, a: 255 });
        let before_root = session.stylesheet_source(node).unwrap().text.clone();
        let before_child = session.stylesheet_source(node).unwrap().imports[1].source.as_ref().unwrap().text.clone();
        let bad = alloc::format!("{} .box{{height:99px}} {}", "@media all {".repeat(33), "}".repeat(33));
        assert!(session.replace_stylesheet_text(node, &bad).is_err());
        assert!(Arc::ptr_eq(&session.stylesheet_source(node).unwrap().text, &before_root));
        assert_eq!(session.document().first_child(node).unwrap(), Some(text_node));
        assert!(matches!(session.document().kind(text_node), Ok(NodeKind::Text(text)) if text == replacement));
        assert!(session.replace_imported_stylesheet_text(node, &[1], &bad).is_err());
        assert!(Arc::ptr_eq(&session.stylesheet_source(node).unwrap().imports[1].source.as_ref().unwrap().text, &before_child));
        let style = session.computed_style(box_node).unwrap();
        assert_eq!(style.width, Some(9.0));
        assert_eq!(style.height, Some(17.0));
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
                form_associated_custom_element:false,
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
                form_associated_custom_element:false,
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
                form_associated_custom_element:false,
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
    fn specification_transparent_block_hit_plane_stays_behind_painted_descendants() {
        for background in ["background:green", "background:transparent"] {
            let source=alloc::format!("<!doctype html><style>#target{{width:100px;height:100px;{background}}}</style><body><div id='target'></div><div id='cover' style='display:none'></div></body>");
            let document=crate::html::parse(&source,32).unwrap();
            let target=crate::selector::get_element_by_id(&document,document.root(),"target").unwrap().unwrap();
            let mut session=RenderSession::new(document);
            session.display_list(800,600,&NoText).unwrap();
            assert_eq!(session.hit_test(58.0,58.0),Some(target),"{background}");
            // A real positioned covering element must still intercept input.
            let cover=crate::selector::get_element_by_id(session.document(),session.document().root(),"cover").unwrap().unwrap();
            session.document_mut().set_attribute(cover,"style","position:absolute;left:8px;top:8px;width:100px;height:100px;z-index:1").unwrap();
            session.display_list(800,600,&NoText).unwrap();
            assert_eq!(session.hit_test(58.0,58.0),Some(cover));
        }
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
    fn specification_window_retained_fragment_corners_preserve_rotation_offscreen_and_freshness() {
        let document=crate::html::parse("<body style='margin:0'><div id='target' style='position:absolute;left:300px;top:400px;width:20px;height:10px;transform:rotate(45deg);transform-origin:0 0'></div></body>",32).unwrap();
        let target=crate::selector::get_element_by_id(&document,document.root(),"target").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(100,80,&NoText).unwrap();
        assert!(session.untransformed_hit_bounds(target).is_none(),"the visible hit API clips this offscreen target");
        let corners=session.client_fragment_corners(target).next().expect("scroll geometry must retain offscreen fragments");
        let embedding=session.content_viewport_transform(target).unwrap();
        let inverse=embedding.inverse().unwrap();
        let local=corners.map(|(x,y)|inverse.apply(x,y));
        for (actual,expected) in local.into_iter().zip([(0.0,0.0),(20.0,0.0),(0.0,10.0),(20.0,10.0)]) {
            assert!((actual.0-expected.0).abs()<0.001&&(actual.1-expected.1).abs()<0.001,"rotated fragment corner {actual:?} must map exactly to {expected:?}");
        }
        session.document_mut().set_attribute(target,"hidden","").unwrap();
        assert!(session.client_fragment_corners(target).next().is_none(),"stale layout cannot supply scroll geometry");
    }

    #[test]
    fn specification_window_scroll_fragment_bounds_ignore_empty_points_and_preserve_first_degenerate_box() {
        let document=crate::html::parse("<div id='target' style='width:20px;height:10px'></div>",32).unwrap();
        let target=crate::selector::get_element_by_id(&document,document.root(),"target").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(100,80,&NoText).unwrap();
        let frame=session.cached.as_mut().unwrap();
        let prototype=frame.geometry.hits.iter().find(|hit|hit.node==target).unwrap().clone();
        frame.geometry.hits.clear();frame.geometry.transforms.clear();
        for rect in [Rect{x:-100.0,y:-100.0,width:0.0,height:0.0},
            Rect{x:10.0,y:20.0,width:20.0,height:10.0},
            Rect{x:40.0,y:30.0,width:0.0,height:60.0}] {
            let mut hit=prototype.clone();hit.rect=rect;frame.geometry.hits.push(hit);
        }
        let corners:Vec<_>=session.client_fragment_corners(target).collect();
        assert_eq!(corners.len(),2,"an empty point cannot enlarge a target with a real box");
        assert_eq!(corners[0][0],(10.0,20.0));
        assert_eq!(corners[1][3],(40.0,90.0),"a degenerate line contributes when another fragment has area");
        session.cached.as_mut().unwrap().geometry.hits[1].rect.width=0.0;
        let corners:Vec<_>=session.client_fragment_corners(target).collect();
        assert_eq!(corners,vec![[(-100.0,-100.0);4]],"when all fragments have a zero dimension use the first fragment");
    }

    #[test]
    fn specification_shared_progress_timeline_owner_geometry_and_scope(){
        use crate::animation::{ProgressRange,progress_timelines as timelines};
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}#port{overflow:auto;width:100px;height:100px;scroll-timeline-name:--track;scroll-timeline-axis:y}#subject{height:20px;margin-top:100px}#tail{height:400px}</style><div id=port><div id=subject></div><div id=tail></div></div>",64).unwrap();
        let port=crate::selector::get_element_by_id(&document,document.root(),"port").unwrap().unwrap();
        let subject=crate::selector::get_element_by_id(&document,document.root(),"subject").unwrap().unwrap();
        let mut session=RenderSession::new(document);session.display_list(200,200,&NoText).unwrap();
        let snapshot=session.animation_snapshot().unwrap();
        let named=timelines::resolve(&mut session,&snapshot,subject,None,"--track",ProgressRange::parse("normal").unwrap()).unwrap();
        assert_eq!(named.source,Some(port));assert!(!named.horizontal);assert_eq!(named.subject,None);
        let before=timelines::sample(&mut session,&named).unwrap();assert_eq!(before.position,0.0);assert_eq!(before.progress,0.0);
        session.set_scroll_offset(port,0.0,50.0).unwrap();session.display_list(200,200,&NoText).unwrap();
        let after=timelines::sample(&mut session,&named).unwrap();assert_eq!(after.position,50.0);assert!(after.progress>before.progress);
        assert_eq!(after.progress,crate::animation::progress_fraction(50.0,after.start,after.end).unwrap());
        let view=timelines::resolve(&mut session,&snapshot,subject,None,"view(y)",ProgressRange::parse("normal").unwrap()).unwrap();
        assert_eq!(view.source,Some(port));assert_eq!(view.subject,Some(subject));assert!(timelines::sample(&mut session,&view).is_some());
        let missing=timelines::resolve(&mut session,&snapshot,subject,None,"--missing",ProgressRange::parse("normal").unwrap()).unwrap();assert!(timelines::sample(&mut session,&missing).is_none());
        let wrong_scope=Some(session.document().root());
        let scoped=timelines::resolve(&mut session,&snapshot,subject,wrong_scope,"--track",ProgressRange::parse("normal").unwrap()).unwrap();assert_eq!(scoped.source,None);
    }

    #[test]
    fn specification_window_scrollport_coordinate_space_tracks_transformed_owner() {
        let document = crate::html::parse("<!doctype html><style>html,body{margin:0}#outer{overflow:auto;width:90px;height:50px}#inner{overflow:auto;width:40px;height:40px;margin-top:100px;transform:scale(2);transform-origin:0 0}#target{height:10px}</style><div id=outer><div id=inner><div style='height:80px'></div><div id=target></div><div style='height:200px'></div></div><div style='height:200px'></div></div>", 64).unwrap();
        let inner = crate::selector::get_element_by_id(&document, document.root(), "inner").unwrap().unwrap();
        let target = crate::selector::get_element_by_id(&document, document.root(), "target").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(200, 200, &NoText).unwrap();
        let frame = session.cached.as_ref().unwrap();
        let port = frame.geometry.scroll_ports.iter().find(|port| port.node == inner).unwrap();
        let owner = port.owner_hit.expect("a transformed element scrollport has an owner box");
        assert_eq!(frame.geometry.hits[owner].node, inner, "owner index remains attached to its box");
        let (rect, inverse) = session.scrollport_coordinate_space(inner).unwrap();
        assert_eq!(inverse.apply(0.0, 260.0), (0.0, 180.0), "port={rect:?}; owner={owner}; transforms={:?}", frame.geometry.transforms.iter().map(|t| (t.hits.clone(), t.matrix)).collect::<Vec<_>>());
        assert_eq!(session.bounding_client_rect(target).unwrap().y, 260.0);
        session.set_scroll_offset(inner, 0.0, 80.0).unwrap();
        session.display_list(200, 200, &NoText).unwrap();
        assert_eq!(session.bounding_client_rect(target).unwrap().y, 100.0);
        session.cached = None;
        session.display_list(200, 200, &NoText).unwrap();
        assert_eq!(session.bounding_client_rect(target).unwrap().y, 100.0);
    }

    #[test]
    fn specification_window_positioned_insets_follow_containing_block_scroll() {
        for (port_style, position) in [("position:relative", "absolute"), ("transform:translate(0px)", "fixed")] {
            let document = crate::html::parse(&alloc::format!("<!doctype html><style>html,body{{margin:0}}#port{{{port_style};margin:70px;width:20px;height:20px;overflow:auto}}#target{{position:{position};left:40px;top:40px;width:10px;height:10px}}#space{{width:100px;height:100px}}</style><div id=port><div id=target></div><div id=space></div></div>"), 64).unwrap();
            let port = crate::selector::get_element_by_id(&document, document.root(), "port").unwrap().unwrap();
            let target = crate::selector::get_element_by_id(&document, document.root(), "target").unwrap().unwrap();
            let mut session = RenderSession::new(document);
            session.display_list(200, 200, &NoText).unwrap();
            assert_eq!(session.bounding_client_rect(target).unwrap().y, 110.0);
            session.set_scroll_offset(port, 30.0, 30.0).unwrap();
            session.display_list(200, 200, &NoText).unwrap();
            let rect = session.bounding_client_rect(target).unwrap();
            assert_eq!((rect.x, rect.y), (80.0, 80.0), "{position} definite insets follow their actual containing block");
            session.cached = None;
            session.display_list(200, 200, &NoText).unwrap();
            assert_eq!(session.bounding_client_rect(target), Some(rect));
        }
    }

    #[test]
    fn specification_window_signed_scroll_bounds_follow_actual_overflow_and_geometry() {
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}html{direction:rtl}#space{height:400px}#target{position:absolute;left:-140px;top:100px;width:20px;height:20px;background:red}</style><div id=space></div><div id=target></div>",32).unwrap();
        let root=document.root();
        let target=crate::selector::get_element_by_id(&document,root,"target").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(100,80,&NoText).unwrap();
        let (min_x,max_x,min_y,max_y)=session.scroll_bounds(root).unwrap();
        assert_eq!((min_x,max_x,min_y),(-140.0,0.0,0.0));
        assert!(max_y>=320.0);
        assert_eq!(session.scroll_extent(root).unwrap().0,140.0);
        assert!(session.set_scroll_offset(root,-140.0,100.0).unwrap());
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(session.scroll_offset(root),(-140.0,100.0));
        assert_eq!(session.hit_test(5.0,5.0),Some(target));
        let bounds=session.bounding_client_rect(target).unwrap();
        assert_eq!((bounds.x,bounds.y),(0.0,0.0));
        assert!(session.set_scroll_offset(root,-130.0,90.0).unwrap());
        session.display_list(100,80,&NoText).unwrap();
        let bounds=session.bounding_client_rect(target).unwrap();
        assert_eq!((bounds.x,bounds.y),(-10.0,10.0));
        assert_eq!(session.hit_test(5.0,15.0),Some(target));
        assert!(session.set_scroll_offset(root,-1000.0,-1000.0).unwrap());
        assert_eq!(session.scroll_offset(root),(-140.0,0.0));
        session.display_list(100,80,&NoText).unwrap();
        assert!(session.set_scroll_offset(root,1000.0,1000.0).unwrap());
        assert_eq!(session.scroll_offset(root),(0.0,max_y));
        session.display_list(100,80,&NoText).unwrap();
        assert_ne!(session.hit_test(5.0,5.0),Some(target));
    }

    #[test]
    fn specification_window_signed_scroll_retained_replay_matches_full_layout() {
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}#port{direction:rtl;width:60px;height:30px;overflow:auto}#space{height:50px}#target{width:120px;height:20px;background:red}</style><div id=port><div id=space></div><div id=target></div><div style='height:30px'></div></div>",32).unwrap();
        let root=document.root();
        let port=crate::selector::get_element_by_id(&document,root,"port").unwrap().unwrap();
        let target=crate::selector::get_element_by_id(&document,root,"target").unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(session.scroll_bounds(port).unwrap().0,-60.0);
        session.set_scroll_offset(port,-60.0,50.0).unwrap();
        session.display_list(100,80,&NoText).unwrap();
        session.set_scroll_offset(port,-55.0,45.0).unwrap();
        assert!(session.cached_display_list().is_some(),"nearby signed movement must reuse the actual command/hit frame");
        let replayed=session.bounding_client_rect(target).unwrap();
        assert_eq!((replayed.x,replayed.y),(-5.0,5.0));
        assert_eq!(session.hit_test(5.0,10.0),Some(target));
        session.cached=None;
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(session.bounding_client_rect(target),Some(replayed));
        assert_eq!(session.hit_test(5.0,10.0),Some(target));
    }

    #[test]
    fn specification_embed_nothing_ignores_descendants_and_document_representation_has_real_viewport_geometry() {
        let mut document=crate::html::parse("<!doctype html><style>html,body{margin:0}embed{display:inline;width:100px;height:40px;background:red;border:0;padding:0}</style><embed id=e>",64).unwrap();
        let e=crate::selector::get_element_by_id(&document,document.root(),"e").unwrap().unwrap();
        let child=document.create(NodeKind::Element{namespace:crate::Namespace::Html,name:crate::Name::new("div"),attributes:Vec::new()}).unwrap();
        document.set_attribute(child,"style","width:999px;height:999px;background:red").unwrap();
        document.append(e,child).unwrap();
        let mut session=RenderSession::new(document);
        session.set_canvas_background(None);
        let list=session.display_list(300,200,&NoText).unwrap().0.clone();
        assert!(session.layout_rect(e).is_none());
        assert!(session.layout_rect(child).is_none(),"embed has no fallback descendants");
        assert_eq!(session.computed_style(e).unwrap().display,css::Display::Inline,"used Nothing does not forge CSSOM display");
        session.set_object_representation(e,crate::object::Representation::Document).unwrap();
        session.display_list(300,200,&NoText).unwrap();
        let rect=session.layout_rect(e).unwrap();
        assert_eq!((rect.width,rect.height),(100.0,40.0));
        assert!(session.layout_rect(child).is_none());
        session.set_object_representation(e,crate::object::Representation::Nothing).unwrap();
        assert_eq!(session.display_list(300,200,&NoText).unwrap().0,list);
        assert!(session.layout_rect(e).is_none());
    }
    #[test]
    fn specification_object_representation_uses_real_image_ratio_document_viewport_and_fallback_geometry() {
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}object{border:0;padding:0}#image{width:40px}#fallback{display:block}#child{width:80px;height:30px;background:red}</style><object id=image><div id=excluded style='width:999px;height:999px'></div></object><object id=doc></object><object id=fallback><div id=child></div></object>",96).unwrap();
        let root=document.root();
        let get=|name|crate::selector::get_element_by_id(&document,root,name).unwrap().unwrap();
        let image=get("image");let doc=get("doc");let fallback=get("fallback");let child=get("child");let excluded=get("excluded");
        let mut session=RenderSession::new(document);
        let bitmap=Arc::new(ImageData{width:2,height:1,pixels:vec![0,128,0,255,0,128,0,255]});
        session.set_object_representation(image,crate::object::Representation::Image).unwrap();
        session.set_node_bitmap(image,Some(bitmap)).unwrap();
        session.set_object_representation(doc,crate::object::Representation::Document).unwrap();
        let first=session.display_list(700,500,&NoText).unwrap().0.clone();
        let rect=session.layout_rect(image).unwrap();assert_eq!((rect.width,rect.height),(40.0,20.0));
        let rect=session.layout_rect(doc).unwrap();assert_eq!((rect.width,rect.height),(300.0,150.0));
        assert!(session.layout_rect(excluded).is_none());
        let rect=session.layout_rect(child).unwrap();assert_eq!((rect.width,rect.height),(80.0,30.0));
        assert_eq!(session.computed_style(image).unwrap().display,css::Display::Inline);
        assert_eq!(session.display_list(700,500,&NoText).unwrap().0,first);
        session.set_object_representation(image,crate::object::Representation::Fallback).unwrap();
        session.set_object_representation(fallback,crate::object::Representation::Document).unwrap();
        session.display_list(700,500,&NoText).unwrap();
        assert!(session.layout_rect(excluded).is_some());assert!(session.layout_rect(child).is_none());
    }

    #[test]
    fn specification_window_inline_iframe_uses_replaced_boxes_with_computed_inline_display() {
        let mut document=crate::html::parse("<!doctype html><style>html,body{margin:0}#sized{width:200px;height:40px;padding:3px;border:2px solid}#automatic{border:0;padding:0}#hidden{display:none}</style><iframe id=sized></iframe><iframe id=automatic></iframe><iframe id=hidden></iframe>",64).unwrap();
        let root=document.root();
        let sized=crate::selector::get_element_by_id(&document,root,"sized").unwrap().unwrap();
        let automatic=crate::selector::get_element_by_id(&document,root,"automatic").unwrap().unwrap();
        let hidden=crate::selector::get_element_by_id(&document,root,"hidden").unwrap().unwrap();
        let fallback=document.create(NodeKind::Element { namespace:crate::Namespace::Html,name:crate::Name::new("div"),attributes:Vec::new() }).unwrap();
        document.set_attribute(fallback,"style","position:absolute;width:999px;height:999px;background:red;z-index:-1").unwrap();
        document.append(sized,fallback).unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(700,500,&NoText).unwrap();
        assert_eq!(session.computed_style(sized).unwrap().display,css::Display::Inline);
        let box_rect=session.layout_rect(sized).unwrap();
        assert_eq!((box_rect.width,box_rect.height),(210.0,50.0));
        let box_rect=session.layout_rect(automatic).unwrap();
        assert_eq!((box_rect.width,box_rect.height),(300.0,150.0));
        assert!(session.layout_rect(fallback).is_none());
        assert!(session.layout_rect(hidden).is_none());
        session.document_mut().set_attribute(sized,"style","box-sizing:border-box;width:220px;height:80px;padding:3px;border:2px solid").unwrap();
        session.display_list(700,500,&NoText).unwrap();
        let box_rect=session.layout_rect(sized).unwrap();
        assert_eq!((box_rect.width,box_rect.height),(220.0,80.0));
        assert_eq!(session.computed_style(sized).unwrap().display,css::Display::Inline);
    }

    #[test]
    fn specification_window_signed_scroll_normalizes_changed_geometry_before_publication() {
        let document=crate::html::parse("<!doctype html><style>html,body{margin:0}html{direction:rtl}#space{height:400px}#target{position:absolute;left:-140px;top:100px;width:20px;height:20px;background:red}</style><div id=space></div><div id=target></div>",32).unwrap();
        let root=document.root();
        let target=crate::selector::get_element_by_id(&document,root,"target").unwrap().unwrap();
        let space=crate::selector::get_element_by_id(&document,root,"space").unwrap().unwrap();
        let html=document.document_element_at(root).unwrap().unwrap();
        let mut session=RenderSession::new(document);
        session.display_list(100,80,&NoText).unwrap();
        session.set_scroll_offset(root,-140.0,100.0).unwrap();
        session.document_mut().set_attribute(html,"style","direction:ltr").unwrap();
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(session.scroll_offset(root),(0.0,100.0));
        assert_eq!(session.bounding_client_rect(target).unwrap().x,-140.0);
        session.document_mut().set_attribute(space,"style","height:0").unwrap();
        session.document_mut().set_attribute(target,"style","left:0;top:0").unwrap();
        session.display_list(100,80,&NoText).unwrap();
        assert_eq!(session.scroll_offset(root),(0.0,0.0));
        let bounds=session.bounding_client_rect(target).unwrap();
        assert_eq!((bounds.x,bounds.y),(0.0,0.0));
        assert_eq!(session.hit_test(5.0,5.0),Some(target));
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
    fn drawable_subtree_snapshot_preserves_border_box() {
        let document = crate::html::parse("<!doctype html><style>body{color:red}#child{width:20px;height:10px;margin:7px;border:2px solid green;transform:translate(50px,50px);background:blue}</style><canvas content=drawable width=100 height=100><div drawable id=child></div></canvas>",128).unwrap();
        let root = crate::selector::query_selector(&document,document.root(),"#child").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        let (list,rect) = session.element_snapshot_display_list(root,100,100,&NoText,None).unwrap();
        assert_eq!(rect,Rect{x:0.0,y:0.0,width:24.0,height:14.0});
        assert!(!list.0.is_empty());
    }

    #[test]
    fn specification_filter_inline_containing_block_uses_own_padding_geometry() {
        for nested in [false, true] {
          for z in ["auto", "-1", "0", "1"] {
            let inner = if nested { "<span><i id=fixed></i><i id=absolute></i></span>" } else { "<i id=fixed></i><i id=absolute></i>" };
            let source = alloc::format!("<style>body{{margin:0}}#parent{{padding:10px}}#host{{border:2px solid transparent;padding:3px;font-size:0;line-height:0}}#content{{display:inline-block;width:24px;height:12px}}#fixed,#absolute{{z-index:{z};position:fixed;left:0;top:0;width:6px;height:6px;background:red}}#absolute{{position:absolute;left:auto;top:auto;right:0;bottom:0}}</style><div id=parent><span id=host><b id=content></b>{inner}</span></div>");
            let find = |document: &Document, selector| crate::selector::query_selector(document, document.root(), selector).unwrap().unwrap();
            let document = crate::html::parse(&source, 64).unwrap();
            let host = find(&document, "#host"); let fixed = find(&document, "#fixed"); let absolute = find(&document, "#absolute");
            let mut session = RenderSession::new(document);
            for effect in ["none", "invert(0)", "invert()", "none"] {
                session.document_mut().set_attribute(host, "style", &alloc::format!("filter:{effect}")).unwrap();
                let list = session.display_list(100, 100, &NoText).unwrap().clone();
                let rect = session.layout_rect(host).unwrap();
                assert_eq!(session.computed_style(host).unwrap().display, crate::css::Display::Inline);
                assert_eq!((rect.width, rect.height), (34.0, 10.0), "nested={nested}/effect={effect}/z={z}: zero font content height plus vertical edges; fragments={:?}", session.client_rects(host));
                let filtered = effect != "none";
                assert_eq!(session.is_viewport_fixed(fixed), !filtered);
                let first = session.layout_rect(fixed).unwrap();
                if filtered {
                    assert_eq!(session.hit_test(first.x + 0.5, first.y + 0.5), Some(fixed), "negative children remain above their own inline's background");
                    let mut filter_scopes = Vec::new();
                    for command in &list.0 {
                        match command {
                            crate::paint::Command::PushLayer { filters, .. } => filter_scopes.push(filters.is_some()),
                            crate::paint::Command::PopLayer => { filter_scopes.pop().unwrap(); },
                            crate::paint::Command::FillRect { color, .. } if *color == (crate::paint::Rgba { r:255,g:0,b:0,a:255 }) => {
                                assert!(filter_scopes.iter().any(|active|*active), "nested={nested}/{effect}/z={z}: positioned paint remains in its real inline filter owner");
                            }
                            _ => {}
                        }
                    }
                }
                assert_eq!((first.x, first.y), if filtered {(rect.x + 2.0, rect.y + 2.0)} else {(0.0,0.0)}, "nested={nested}/{effect}");
                let last = session.layout_rect(absolute).unwrap();
                assert_eq!((last.x, last.y), if filtered {(rect.x + rect.width - 8.0, rect.y + rect.height - 8.0)} else {(94.0,94.0)}, "nested={nested}/{effect}");
                let mut document = crate::html::parse(&source, 64).unwrap();
                let fresh_host = find(&document, "#host"); document.set_attribute(fresh_host, "style", &alloc::format!("filter:{effect}")).unwrap();
                let mut fresh = RenderSession::new(document);
                assert_eq!(list, fresh.display_list(100,100,&NoText).unwrap().clone(), "nested={nested}/{effect}: retained/fresh");
            }
          }
        }
    }

    #[test]
    fn specification_inline_filter_containing_block_uses_asymmetric_signed_edges() {
        let source = "<style>body{margin:0}#parent{padding:10px}#host{filter:invert(0);margin:19px -2px 23px -4px;padding:1px 7px 5px 3px;border:solid transparent;border-width:2px 4px 6px 8px;font-size:0;line-height:0}b{display:inline-block;width:24px;height:12px}i{position:fixed;left:0;top:0;width:6px;height:6px}#absolute{position:absolute;left:auto;top:auto;right:0;bottom:0}</style><div id=parent><span id=host><b></b><i id=fixed></i><i id=absolute></i></span></div>";
        let document = crate::html::parse(source,64).unwrap();
        let find = |selector| crate::selector::query_selector(&document,document.root(),selector).unwrap().unwrap();
        let host=find("#host"); let fixed=find("#fixed"); let absolute=find("#absolute");
        let mut session=RenderSession::new(document);
        session.display_list(100,100,&NoText).unwrap();
        assert_eq!(session.layout_rect(host),Some(crate::paint::Rect{x:6.0,y:19.0,width:46.0,height:14.0}),
            "signed inline margins and asymmetric padding/borders define the actual border box");
        assert_eq!(session.layout_rect(fixed),Some(crate::paint::Rect{x:14.0,y:21.0,width:6.0,height:6.0}));
        assert_eq!(session.layout_rect(absolute),Some(crate::paint::Rect{x:42.0,y:21.0,width:6.0,height:6.0}));
    }

    #[test]
    fn specification_inline_font_edges_do_not_enlarge_atomic_child_line() {
        for effect in ["none","invert(0)"] {
            let source=alloc::format!("<style>body{{margin:0;font-size:0;line-height:0}}#host{{filter:{effect};border:2px solid green;padding:3px;background:lime}}b{{display:inline-block;width:24px;height:12px}}#after{{height:1px}}</style><div><span id=host><b></b></span></div><div id=after></div>");
            let document=crate::html::parse(&source,64).unwrap();
            let find=|selector|crate::selector::query_selector(&document,document.root(),selector).unwrap().unwrap();
            let host=find("#host");let after=find("#after");
            let mut session=RenderSession::new(document);let list=session.display_list(100,100,&NoText).unwrap().clone();
            assert_eq!(session.layout_rect(host),Some(crate::paint::Rect{x:0.0,y:7.0,width:34.0,height:10.0}),
                "{effect}: zero font strut anchors its own paint box to the atomic child's 12px baseline");
            assert_eq!(session.layout_rect(after).unwrap().y,12.0,
                "{effect}: vertical inline edges paint outside the line's 12px layout contribution");
            assert!(list.0.iter().any(|command|match command {
                crate::paint::Command::FillRect{rect,color} => *color==crate::paint::Rgba{r:0,g:255,b:0,a:255}
                    && *rect==crate::paint::Rect{x:0.0,y:7.0,width:34.0,height:10.0},
                _=>false,
            }),"{effect}: the actual background command shares the font-based geometry");
        }
    }

    #[test]
    fn specification_inline_filter_resolves_own_percentage_edges_against_containing_block() {
        for effect in ["none", "invert(0)"] {
            let source=alloc::format!("<style>body{{margin:0;font-size:0;line-height:0}}#parent{{width:100px}}#host{{filter:{effect};margin:19px -2px 23px -4px;padding:1% 7% 5% 3%;border:solid transparent;border-width:2px 4px 6px 8px}}b{{display:inline-block;width:24px;height:12px}}i{{position:fixed;left:0;top:0;width:6px;height:6px}}#absolute{{position:absolute;left:auto;top:auto;right:0;bottom:0}}</style><div id=parent><span id=host><b></b><i id=fixed></i><i id=absolute></i></span></div>");
            let document=crate::html::parse(&source,64).unwrap();
            let find=|selector|crate::selector::query_selector(&document,document.root(),selector).unwrap().unwrap();
            let host=find("#host");let fixed=find("#fixed");let absolute=find("#absolute");
            let mut session=RenderSession::new(document);session.display_list(200,100,&NoText).unwrap();
            assert_eq!(session.layout_rect(host),Some(Rect{x:-4.0,y:9.0,width:46.0,height:14.0}),
                "{effect}: all percentage padding uses the 100px containing inline size, while vertical margins do not shift the inline");
            if effect!="none" {
                assert_eq!(session.layout_rect(fixed),Some(Rect{x:4.0,y:11.0,width:6.0,height:6.0}));
                assert_eq!(session.layout_rect(absolute),Some(Rect{x:32.0,y:11.0,width:6.0,height:6.0}));
            }
        }
    }

    #[test]
    fn specification_filters_establish_nonroot_containing_blocks_and_retained_groups() {
        for host in ["div", "fieldset"] {
            let source = alloc::format!("<style>body{{margin:0}}#host{{margin:20px;border:3px solid transparent;padding:5px;width:40px;height:30px}}#fixed,#absolute{{position:fixed;left:0;top:0;width:6px;height:6px;background:red}}#absolute{{position:absolute;left:9px}}</style><{host} id=host><div id=fixed></div><div id=absolute></div></{host}>");
            let document = crate::html::parse(&source, 64).unwrap();
            let find = |document: &Document, selector| crate::selector::query_selector(document, document.root(), selector).unwrap().unwrap();
            let node = find(&document, "#host"); let fixed = find(&document, "#fixed"); let absolute = find(&document, "#absolute");
            let mut session = RenderSession::new(document);
            for effect in ["none", "invert(0)", "invert()", "none"] {
                session.document_mut().set_attribute(node, "style", &alloc::format!("filter:{effect}")).unwrap();
                let retained = session.display_list(100, 100, &NoText).unwrap().clone();
                let filtered = effect != "none";
                assert_eq!(session.is_viewport_fixed(fixed), !filtered, "{host}/{effect}");
                let expected = if filtered { (23.0, 23.0) } else { (0.0, 0.0) };
                let rect = session.layout_rect(fixed).unwrap();
                assert_eq!((rect.x, rect.y), expected, "{host}/{effect}");
                let rect = session.layout_rect(absolute).unwrap();
                assert_eq!((rect.x, rect.y), if filtered { (32.0, 23.0) } else { (9.0, 0.0) }, "{host}/{effect}");
                assert_eq!(retained.0.iter().filter(|command| matches!(command, crate::paint::Command::PushLayer { filters: Some(_), .. })).count(), usize::from(filtered));
                let mut fresh_document = crate::html::parse(&source, 64).unwrap();
                let fresh_node = find(&fresh_document, "#host");
                fresh_document.set_attribute(fresh_node, "style", &alloc::format!("filter:{effect}")).unwrap();
                let mut fresh = RenderSession::new(fresh_document);
                assert_eq!(retained, fresh.display_list(100, 100, &NoText).unwrap().clone(), "{host}/{effect} retained matches fresh");
            }
        }
        let document = crate::html::parse("<html style='filter:invert()'><body style='margin:20px'><div id=fixed style='position:fixed;left:0;top:0;width:6px;height:6px;background:red'></div></body></html>", 32).unwrap();
        let fixed = crate::selector::query_selector(&document, document.root(), "#fixed").unwrap().unwrap();
        let mut session = RenderSession::new(document);
        session.display_list(100, 100, &NoText).unwrap();
        assert!(session.is_viewport_fixed(fixed), "the document root filter does not replace the viewport CB");
        let rect = session.layout_rect(fixed).unwrap(); assert_eq!((rect.x, rect.y), (0.0, 0.0));
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
    fn query_container_context_uses_measured_per_axis_sizes_and_unknown_axes() {
        let document = crate::html::parse(
            "<style>html,body{margin:0}#outer{container-type:size;width:200px;height:100px}#middle{container-type:inline-size;width:120px;height:40px}#vertical{container-type:inline-size;writing-mode:vertical-rl;width:70px;height:60px}</style><div id=outer><div id=middle><div id=target></div></div><div id=vertical><div id=vertical-target></div></div></div><div id=fallback></div>",
            128,
        )
        .unwrap();
        let find = |selector| {
            crate::selector::query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let target = find("#target");
        let vertical_target = find("#vertical-target");
        let fallback = find("#fallback");
        let mut session = RenderSession::new(document);

        let unresolved = session.query_container_context(target).unwrap();
        assert_eq!(unresolved.width, css::ContainerUnitBasis::Unknown);
        assert_eq!(unresolved.height, css::ContainerUnitBasis::Unknown);

        session.display_list(300, 200, &NoText).unwrap();
        let horizontal = session.query_container_context(target).unwrap();
        assert_eq!(horizontal.width, css::ContainerUnitBasis::Size(120.0));
        assert_eq!(horizontal.height, css::ContainerUnitBasis::Size(100.0));
        assert_eq!(horizontal.inline, css::ContainerUnitBasis::Size(120.0));
        assert_eq!(horizontal.block, css::ContainerUnitBasis::Size(100.0));

        let vertical = session.query_container_context(vertical_target).unwrap();
        assert_eq!(vertical.width, css::ContainerUnitBasis::Size(200.0));
        assert_eq!(vertical.height, css::ContainerUnitBasis::Size(60.0));
        assert_eq!(vertical.inline, css::ContainerUnitBasis::Size(60.0));
        assert_eq!(vertical.block, css::ContainerUnitBasis::Size(200.0));

        let fallback = session.query_container_context(fallback).unwrap();
        assert_eq!(fallback.width, css::ContainerUnitBasis::NoContainer);
        assert_eq!(fallback.height, css::ContainerUnitBasis::NoContainer);
        assert_eq!(fallback.inline, css::ContainerUnitBasis::NoContainer);
        assert_eq!(fallback.block, css::ContainerUnitBasis::NoContainer);
        assert_eq!(fallback.small_viewport.width, 300.0);
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
            color_schemes:Default::default(),
            svg_fragment:None, svg_image_viewport:None,
            paint_definitions: &[],
            paint_registration_revision:0,
            bitmaps: &[],
            embedded:&[],
            objects: &[],
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
    #[test]
    fn specification_xml_stylesheet_retained_metadata_admission_tracks_live_and_detached_sheets() {
        let document=crate::xml::parse("<?xml-stylesheet href='one.css'?><root/>",128).unwrap();
        let owner=document.first_child(document.root()).unwrap().unwrap();
        let mut session=RenderSession::new(document);
        let source=||css::StylesheetSource{disabled:false, url:Arc::from("https://xml-style.test/one.css"),text:Arc::from("root{color:green}"),imports:Vec::new()};
        session.set_stylesheet_source(owner,Some(source())).unwrap();
        session.set_stylesheet_metadata(owner,&"A".repeat(css::MAX_CSS_BYTES-256),"screen").unwrap();
        let first=session.stylesheet_root_lease(owner).unwrap();
        session.set_stylesheet_source(owner,None).unwrap();
        assert!(first.is_detached());
        session.set_stylesheet_source(owner,Some(source())).unwrap();
        assert!(session.set_stylesheet_metadata(owner,&"B".repeat(256),"").is_err(),"retained old sheets remain charged");
        assert_eq!(first.metadata().unwrap().media.borrow().as_ref(),"screen");
        drop(first);
        session.set_stylesheet_metadata(owner,&"B".repeat(256),"").unwrap();
        let second=session.stylesheet_root_lease(owner).unwrap();
        session.set_stylesheet_media(owner,second.metadata().unwrap(),"print").unwrap();
        assert_eq!(second.metadata().unwrap().media.borrow().as_ref(),"print");
    }

    #[test]
    fn specification_svg_view_session_live_named_view_and_fragment_rebind_share_ratio() {
        let document=crate::xml::parse("<svg xmlns='http://www.w3.org/2000/svg' width='100' viewBox='0 0 2 1'><view id='selected' viewBox='0 0 1 1'/></svg>",64).unwrap();
        let mut session=RenderSession::new(document);
        let natural=|session:&mut RenderSession|session.embedded_document_intrinsic_size(false,Some(&NoText)).unwrap().unwrap().default_dimensions();
        assert_eq!(natural(&mut session),(100.0,50.0));
        session.set_svg_fragment(Some("selected")).unwrap();
        assert_eq!(natural(&mut session),(100.0,100.0));
        let view=crate::selector::get_element_by_id(session.document(),session.document().root(),"selected").unwrap().unwrap();
        session.document_mut().set_attribute(view,"viewBox","0 0 4 1").unwrap();
        assert_eq!(natural(&mut session),(100.0,25.0));
        session.set_svg_fragment(Some("svgView%28viewBox%280%2C0%2C1%2C2%29%29")).unwrap();
        assert_eq!(natural(&mut session),(100.0,200.0));
        session.set_svg_fragment(None).unwrap();
        assert_eq!(natural(&mut session),(100.0,50.0));
        assert!(session.set_svg_fragment(Some(&"x".repeat(8193))).is_err());
        assert_eq!(natural(&mut session),(100.0,50.0));
    }

    #[test]
    fn specification_embed_svg_intrinsic_computed_dimensions_ratio_and_live_source_geometry() {
        for (source, natural, dimensions) in [
            ("<svg xmlns='http://www.w3.org/2000/svg' width='100' height='80' viewBox='0 0 9 1'/>",crate::object::IntrinsicSize{width:Some(100.0),height:Some(80.0),ratio:Some(1.25)},(100.0,80.0)),
            ("<svg xmlns='http://www.w3.org/2000/svg' width='100%' height='50%' viewBox='0 0 200 200'/>",crate::object::IntrinsicSize{width:None,height:None,ratio:Some(1.0)},(150.0,150.0)),
            ("<svg xmlns='http://www.w3.org/2000/svg' width='100' viewBox='0 0 2 1'/>",crate::object::IntrinsicSize{width:Some(100.0),height:None,ratio:Some(2.0)},(100.0,50.0)),
            ("<svg xmlns='http://www.w3.org/2000/svg' width='100' height='80' style='width:auto;height:auto'/>",crate::object::IntrinsicSize::default(),(300.0,150.0)),
            ("<svg xmlns='http://www.w3.org/2000/svg' width='100' style='width:calc(10% + 30px)' viewBox='0 0 2 1'/>",crate::object::IntrinsicSize{width:None,height:None,ratio:Some(2.0)},(300.0,150.0)),
        ] {
            let document=crate::xml::parse(source,64).unwrap();
            let mut child=RenderSession::new(document);
            let actual=child.embedded_document_intrinsic_size(false,Some(&NoText)).unwrap().unwrap();
            assert_eq!(actual,natural,"{source}");
            assert_eq!(actual.default_dimensions(),dimensions);
            let document=crate::html::parse("<!doctype html><style>body{margin:0}embed{display:block}</style><embed id=e>",64).unwrap();
            let node=crate::selector::query_selector(&document,document.root(),"#e").unwrap().unwrap();
            let mut owner=RenderSession::new(document);
            owner.set_object_representation(node,crate::object::Representation::Document).unwrap();
            owner.set_embedded_intrinsic_size(node,Some(actual)).unwrap();
            owner.display_list(800,600,&NoText).unwrap();
            let rect=owner.layout_rect(node).unwrap();
            assert_eq!((rect.width,rect.height),dimensions);
            owner.set_embedded_intrinsic_size(node,Some(crate::object::IntrinsicSize{width:Some(40.0),height:Some(20.0),ratio:Some(2.0)})).unwrap();
            owner.display_list(800,600,&NoText).unwrap();
            let rect=owner.layout_rect(node).unwrap();
            assert_eq!((rect.width,rect.height),(40.0,20.0));
            owner.set_object_representation(node,crate::object::Representation::Nothing).unwrap();
            assert!(owner.set_embedded_intrinsic_size(node,Some(actual)).is_err());
        }
        let document=crate::html::parse("<!doctype html><img id=image>",32).unwrap();
        let image=crate::selector::query_selector(&document,document.root(),"#image").unwrap().unwrap();
        let mut raster=RenderSession::new(document);
        assert!(raster.embedded_document_intrinsic_size(true,None).unwrap().is_none());
        raster.set_node_bitmap(image,Some(Arc::new(ImageData{width:3,height:2,pixels:alloc::vec![0;24]}))).unwrap();
        let natural=raster.embedded_document_intrinsic_size(true,None).unwrap().unwrap();
        assert_eq!(natural.default_dimensions(),(3.0,2.0));
        assert_eq!(natural.ratio,Some(1.5));

    }

}
