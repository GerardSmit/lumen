//! Host-backed image request state for HTMLImageElement.
//!
//! Resource bytes and decoding remain the embedder's responsibility through
//! `ImageResolver`. This module tracks current and pending image requests,
//! publishes the current decoded bitmap to the render session, and leaves
//! event dispatch to the realm's user-agent task queue.

use lumen_html::{
    Document, Namespace, NodeId, NodeKind,
    layout::{ImageResolver, ImageState},
    observe::{ObservedKind, ObservedMutation},
    paint::ImageData,
};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::Arc,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageEventKind {
    Load,
    Error,
}

impl ImageEventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Load => "load",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct QueuedImageEvent {
    pub(crate) node: NodeId,
    pub(crate) generation: u64,
    pub(crate) kind: ImageEventKind,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ImageSnapshot {
    pub(crate) complete: bool,
    pub(crate) current_src: String,
    pub(crate) natural_width: u32,
    pub(crate) natural_height: u32,
    /// Exact committed dimensions precede the unsigned WebIDL projection.
    pub(crate) natural_size: Option<(f64,f64)>,
}

/// The image source state exposed to canvas consumers. Pending or absent
/// requests are unusable without being broken; a decoded current image stays
/// usable while a replacement request is pending.
pub(crate) enum CanvasImageState {
    Available(Arc<ImageData>, bool),
    Unavailable,
    Broken,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum RequestState {
    #[default]
    Unavailable,
    Loading,
    Available,
    Broken,
}

struct ImageRequest {
    current_src: String,
    generation: u64,
    state: RequestState,
    image: Option<Arc<ImageData>>,
    intrinsic:Option<lumen_html::object::IntrinsicSize>,
    density:f64,
    force_error: bool,
    event_queued: bool,
    origin_clean: bool,
    response_policy:Option<ResponsePolicy>,
}

impl Default for ImageRequest {
    fn default()->Self {Self{current_src:String::new(),generation:0,state:RequestState::Unavailable,image:None,intrinsic:None,density:1.0,force_error:false,event_queued:false,origin_clean:false,response_policy:None}}
}
impl ImageRequest {
    fn loading(current_src: String, generation: u64, force_error: bool) -> Self {
        Self {
            current_src,
            generation,
            state: RequestState::Loading,
            image: None,
            intrinsic:None,
            density:1.0,
            force_error,
            event_queued: false,
            origin_clean: false,
            response_policy:None,
        }
    }
}

#[derive(Default)]
struct Request {
    // The outer option records whether DOM attributes have been observed yet;
    // its inner option distinguishes an omitted src from src="".
    selected_source: Option<Option<String>>,
    selected_crossorigin: Option<Option<bool>>,
    selected_density:Option<f64>,
    observed_version:Option<u64>,
    environment_dirty:bool,
    auto_width:Option<f32>,
    selection_dirty: bool,
    selection_generation: u64,
    base: String,
    current: ImageRequest,
    pending: Option<ImageRequest>,
    // A completed request can satisfy a later source update from the
    // document's available-image list. That update still queues its own load
    // event, but does not need to resolve or decode the URL again.
    ready_event: Option<ImageEventKind>,
    published_image: Option<Arc<ImageData>>,
    published_metadata:Option<lumen_html::responsive_images::ImageMetadata>,
}

#[derive(Default)]
pub(crate) struct ImageLoader {
    resolver: RefCell<Option<Rc<dyn ImageResolver>>>,
    policy: RefCell<Option<Rc<dyn Fn(&str)->Result<lumen_common::csp::Decision,lumen_common::csp::Error>>>>,
    response_policy:RefCell<Option<Rc<dyn Fn()->ResponsePolicy>>>,
    policy_violations: RefCell<Vec<lumen_common::csp::Violation>>,
    policy_overflow: Cell<bool>,
    selection_environment:Cell<Option<(lumen_html::css::MediaEnvironment,f64)>>,
    auto_inputs_epoch:Cell<Option<(u64,u64,usize,u64)>>,
    requests: RefCell<HashMap<NodeId, Request>>,
    bitmap_updates: RefCell<HashMap<NodeId, (Option<Arc<ImageData>>,lumen_html::responsive_images::ImageMetadata)>>,
    next_generation: Cell<u64>,
    tree_generation: Cell<u64>,
    discovery: RefCell<Discovery>,
    needs_scan: Cell<bool>,
    has_dirty: Cell<bool>,
    loading_remaining: Cell<bool>,
    polled_generation: Cell<Option<u64>>,
    pending_scratch: RefCell<Vec<NodeId>>,
}

/// Image discovery results reused while the tree, image attributes and base
/// URL are unchanged, together with scratch buffers for the completion pump.
#[derive(Default)]
struct Discovery {
    key: Option<(u64, usize)>,
    base: String,
    nodes: Vec<NodeId>,
    seen: HashSet<NodeId>,
    stale: Vec<NodeId>,
    pending: Vec<PendingLoad>,
}

struct PendingLoad {
    node: NodeId,
    generation: u64,
    current_src: String,
    base: String,
    force_error: bool,
    response_policy:Option<ResponsePolicy>,
}
type ResponsePolicy=Rc<dyn Fn(&str,&str,u32)->Result<lumen_common::csp::Decision,lumen_common::csp::Error>>;

impl ImageLoader {
    pub(crate) fn invalidate_environment(&self) {
        for request in self.requests.borrow_mut().values_mut(){request.environment_dirty=true;}
        self.has_dirty.set(true);self.mark_scan_needed();
    }
    /// Rendering supplies all selection inputs. Warm requests reuse their
    /// parsed/request state until actual environment or auto-size inputs change.
    pub(crate) fn configure_selection(&self,session:&mut lumen_html::session::RenderSession,dpr:f64,base:&str) {
        self.refresh_discovery(session.document(),base);
        let environment=session.media_environment();
        if self.selection_environment.replace(Some((environment,dpr)))!=Some((environment,dpr)){self.invalidate_environment();}
        let epoch=(session.document().version(),session.frame_id(),self.requests.borrow().len(),self.tree_generation.get());
        if self.auto_inputs_epoch.get()==Some(epoch){return;}
        let nodes=self.requests.borrow().keys().copied().collect::<Vec<_>>();
        for node in nodes {
            let width=if session.document().is_connected_element(node){session.image_auto_sizes_width(node)}else{None};
            let mut requests=self.requests.borrow_mut();let Some(request)=requests.get_mut(&node)else{continue};
            let next=if session.document().is_connected_element(node){width.or(request.auto_width)}else{None};
            if next!=request.auto_width {request.auto_width=next;request.environment_dirty=true;self.has_dirty.set(true);self.mark_scan_needed();}
        }
        self.auto_inputs_epoch.set(Some((session.document().version(),session.frame_id(),self.requests.borrow().len(),self.tree_generation.get())));
    }
    pub(crate) fn set_policy(&self,policy:Rc<dyn Fn(&str)->Result<lumen_common::csp::Decision,lumen_common::csp::Error>>) {
        *self.policy.borrow_mut()=Some(policy);
    }
    pub(crate) fn set_response_policy(&self,policy:Rc<dyn Fn()->ResponsePolicy>){*self.response_policy.borrow_mut()=Some(policy);}
    pub(crate) fn take_policy_violations(&self)->Result<Vec<lumen_common::csp::Violation>,lumen_common::csp::Error> {
        if self.policy_overflow.replace(false) {self.policy_violations.borrow_mut().clear();return Err(lumen_common::csp::Error::Capacity)}
        Ok(std::mem::take(&mut *self.policy_violations.borrow_mut()))
    }
    fn policy_blocks(&self,source:&str)->bool {
        let policy=self.policy.borrow().clone();let Some(policy)=policy else{return false};
        self.record_policy_decision(policy(source))
    }
    fn record_policy_decision(&self,result:Result<lumen_common::csp::Decision,lumen_common::csp::Error>)->bool {
        let decision=match result {Ok(decision)=>decision,Err(lumen_common::csp::Error::Capacity)=>{self.policy_overflow.set(true);return true},Err(lumen_common::csp::Error::InvalidUrl)=>return true};
        let mut pending=self.policy_violations.borrow_mut();
        // Bound pending reporting metadata while getter-driven source selection
        // runs without an engine task context. Exhaustion fails closed.
        if pending.len().saturating_add(decision.violations.len())>256 {self.policy_overflow.set(true);return true}
        pending.extend(decision.violations);decision.blocked
    }
    pub(crate) fn set_resolver(&self, resolver: Rc<dyn ImageResolver>) {
        let mut installed = self.resolver.borrow_mut();
        if installed
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, &resolver))
        {
            return;
        }
        *installed = Some(resolver);
        drop(installed);
        self.mark_scan_needed();

        for request in self.requests.borrow_mut().values_mut() {
            if request.current.state == RequestState::Loading {
                request.current.generation = self.allocate_generation();
            }
            if let Some(pending) = request.pending.as_mut() {
                if pending.state == RequestState::Loading {
                    pending.generation = self.allocate_generation();
                }
            }
        }
    }

    pub(crate) fn on_mutation(&self, document: &Document, mutation: &ObservedMutation) {
        match &mutation.kind {
            ObservedKind::TextSplit { .. } | ObservedKind::TextMerge { .. } | ObservedKind::SlotAssignment | ObservedKind::ChildListReplacement { .. } => {}
            ObservedKind::ChildList { .. } | ObservedKind::ChildListMany { .. } => {
                self.bump_tree_generation();self.mark_scan_needed();
                self.note_picture_change(document,mutation.target);
                for node in mutation.kind.added_nodes().chain(mutation.kind.removed_nodes()){if is_html_image(document,node){self.note_source_change(document,node);}}
            }
            ObservedKind::Attribute {
                name,
                old_value,
                namespace_uri,
            } if is_html_image(document, mutation.target) => {
                if namespace_uri.is_some() {
                    return;
                }
                if name.eq_ignore_ascii_case("src") {
                    self.note_source_change(document, mutation.target);
                } else if ["srcset", "sizes", "referrerpolicy", "decoding", "loading"]
                    .iter()
                    .any(|relevant| name.eq_ignore_ascii_case(relevant))
                {
                    self.note_source_change(document,mutation.target);
                } else if name.eq_ignore_ascii_case("crossorigin")
                    && crossorigin_state(old_value.as_deref())
                        != image_crossorigin(document, mutation.target)
                {
                    // crossorigin is relevant when its parsed state changes,
                    // not merely when its raw spelling is different.
                    self.note_source_change(document, mutation.target);
                }
            }
            ObservedKind::Attribute{name,namespace_uri,..} if namespace_uri.is_none()
                &&matches!(document.kind(mutation.target),Ok(NodeKind::Element{namespace:Namespace::Html,name:tag,..})if tag=="source")
                &&["srcset","sizes","media","type","width","height"].iter().any(|attribute|name.eq_ignore_ascii_case(attribute))=>{
                if let Some(parent)=document.parent(mutation.target).ok().flatten(){self.note_picture_change(document,parent);}
            }
            _ => {}
        }
    }
    fn note_picture_change(&self,document:&Document,parent:NodeId) {
        if !matches!(document.kind(parent),Ok(NodeKind::Element{namespace:Namespace::Html,name,..})if name=="picture"){return;}
        let mut current=document.first_child(parent).ok().flatten();let mut remaining=document.node_count();
        while let Some(node)=current {if remaining==0{break;}remaining-=1;current=document.next_sibling(node).ok().flatten();if is_html_image(document,node){self.note_source_change(document,node);}}
    }

    fn note_source_change(&self, document: &Document, node: NodeId) {
        if !is_html_image(document, node) {
            return;
        }
        let mut requests = self.requests.borrow_mut();
        // The first src mutation often happens on a detached `new Image()`.
        // Register it here so the host pump can retain and resolve it even
        // though it is not reachable from the document tree.
        let request = requests.entry(node).or_default();
        // Capture the effective base when the source update is processed,
        // rather than reusing the previous request's base. Even assigning the
        // same relative src can select a new URL after a base change.
        request.selection_dirty = true;
        self.has_dirty.set(true);
        self.bump_tree_generation();
        self.mark_scan_needed();
        // Invalidate any completion task already queued for the previous
        // selection immediately. The next synchronization either preserves
        // its in-flight request or stages a fresh cached completion.
        request.selection_generation = self.allocate_generation();
    }

    pub(crate) fn has_dirty_source(&self, node: NodeId) -> bool {
        self.requests
            .borrow()
            .get(&node)
            .is_some_and(|request| request.selection_dirty||request.environment_dirty)
    }

    pub(crate) fn has_dirty_requests(&self) -> bool {
        self.has_dirty.get()
    }

    /// Selects sources for requests whose src or crossorigin changed, so the
    /// current bitmap is replaced or cleared before it is next published.
    pub(crate) fn synchronize_dirty(&self, document: &Document, base: &str) {
        if !self.has_dirty.replace(false) {
            return;
        }
        let dirty = self
            .requests
            .borrow()
            .iter()
            .filter_map(|(&node, request)| (request.selection_dirty||request.environment_dirty||request.observed_version.is_none()).then_some(node))
            .collect::<Vec<_>>();
        for node in dirty {
            self.synchronize_node(document, node, base);
        }
    }

    fn synchronize_node(&self, document: &Document, node: NodeId, base: &str) -> bool {
        if !is_html_image(document,node){return false;}
        if self.requests.borrow().get(&node).is_some_and(|request|!request.selection_dirty&&!request.environment_dirty&&request.observed_version==Some(document.version())&&request.base==base&&request.selected_source.is_some()){return true;}
        let (auto_width,deferred)=self.requests.borrow().get(&node).map_or((None,false),|request|(request.auto_width,request.environment_dirty&&!request.selection_dirty&&request.pending.is_some()));
        // HTML's environment algorithm leaves an in-flight pending selection
        // alone. Relevant source mutations can still replace that request.
        if deferred{return true;}
        let selection=if let Some((environment,dpr))=self.selection_environment.get(){
            match lumen_html::responsive_images::select_for_device(document,node,environment,dpr,auto_width) {
                Ok(Some(selection))=>selection,
                Ok(None)=>return false,
                Err(_)=>lumen_html::responsive_images::Selection{source:Some(String::new()),density:1.0,dimension_source:node},
            }
        }else{
            let Some(source)=image_source(document,node)else{return false};
            lumen_html::responsive_images::Selection{source,density:1.0,dimension_source:node}
        };
        let crossorigin=image_crossorigin(document,node);
        let mut requests=self.requests.borrow_mut();let request=requests.entry(node).or_default();
        request.observed_version=Some(document.version());
        if !request.selection_dirty&&request.selected_source.as_ref().is_some_and(|selected|selected==&selection.source)
            &&request.selected_density==Some(selection.density)&&request.selected_crossorigin==Some(crossorigin) {
            request.environment_dirty=false;return true;
        }
        request.environment_dirty=false;
        self.select_source(request,node,selection.source,selection.density,crossorigin,base);
        true
    }

    fn select_source(
        &self,
        request: &mut Request,
        node: NodeId,
        source: Option<String>,
        density:f64,
        crossorigin: Option<bool>,
        base: &str,
    ) {
        request.selection_dirty = false;
        request.selected_density=Some(density);
        self.mark_scan_needed();
        let cors_mode_changed = request.selected_crossorigin != Some(crossorigin);
        let Some(source) = source else {
            request.selection_generation = self.allocate_generation();
            request.ready_event = None;
            request.selected_source = Some(None);
            request.selected_crossorigin = Some(crossorigin);
            request.base = base.to_owned();
            request.current = ImageRequest::default();
            request.pending = None;
            self.stage_current_image(request, node);
            return;
        };

        let current_src = if source.is_empty() {
            String::new()
        } else {
            resolved_url(&source, base)
        };
        let force_error = source.is_empty() || current_src.is_empty();

        if let Some(pending) = request.pending.as_mut() {
            if !cors_mode_changed
                && pending.current_src == current_src
                && pending.force_error == force_error
            {
                // Updating image data step 14 returns for an unchanged pending
                // URL. Its pixel density remains captured by that request; the
                // later source mutation must not prepare it again.
                // The in-flight request already represents this selection.
                // Keep it alive, while invalidating any event queued for the
                // previous current request.
                request.selection_generation = self.allocate_generation();
                request.ready_event = None;
                request.selected_source = Some(Some(source));
                request.selected_crossorigin = Some(crossorigin);
                request.base = base.to_owned();
                return;
            }
        }

        // Actual request preparation checks CSP before either provider I/O or
        // reuse of an available bitmap; an unchanged pending fetch keeps its
        // already captured policy decision. Selection generation invalidates
        // stale load/error events through the existing image state machine.
        let force_error = force_error || (!current_src.is_empty() && self.policy_blocks(&current_src));

        if !cors_mode_changed
            && !force_error
            && request.current.state == RequestState::Available
            && request.current.current_src == current_src
        {
            // This is the available-image fast path: reuse the decoded bitmap,
            // but give this source update a fresh event generation. Any event
            // queued for an earlier selection becomes stale, and the new load
            // event is queued by the next host pump without resolving again.
            request.selection_generation = self.allocate_generation();
            request.ready_event = Some(ImageEventKind::Load);
            request.current.event_queued = true;
            request.current.density=density;
            request.selected_source = Some(Some(source));
            request.selected_crossorigin = Some(crossorigin);
            request.base = base.to_owned();
            request.pending = None;
            self.stage_current_image(request, node);
            return;
        }

        request.selection_generation = self.allocate_generation();
        request.ready_event = None;
        self.mark_scan_needed();
        request.selected_source = Some(Some(source.clone()));
        request.selected_crossorigin = Some(crossorigin);
        request.base = base.to_owned();
        let mut next = ImageRequest::loading(current_src, self.allocate_generation(), force_error);
        next.density=density;
        next.response_policy=self.response_policy.borrow().as_ref().map(|capture|capture());
        if request.current.state == RequestState::Available {
            request.pending = Some(next);
        } else {
            request.current = next;
            request.pending = None;
        }
        self.stage_current_image(request, node);
    }

    fn stage_current_image(&self,request:&mut Request,node:NodeId) {
        let metadata=lumen_html::responsive_images::ImageMetadata{intrinsic:request.current.intrinsic,density:request.current.density,
            source:(!request.current.current_src.is_empty()).then(||Arc::from(request.current.current_src.as_str()))};
        if same_image(request.published_image.as_ref(),request.current.image.as_ref())&&request.published_metadata.as_ref()==Some(&metadata){return;}
        let image=request.current.image.clone();request.published_image=image.clone();request.published_metadata=Some(metadata.clone());
        self.bitmap_updates.borrow_mut().insert(node,(image,metadata));
    }
    pub(crate) fn take_bitmap_updates(&self)->Vec<(NodeId,Option<Arc<ImageData>>,lumen_html::responsive_images::ImageMetadata)> {
        self.bitmap_updates.borrow_mut().drain().map(|(node,(image,metadata))|(node,image,metadata)).collect()
    }

    pub(crate) fn snapshot(&self, document: &Document, node: NodeId, base: &str,scheme:lumen_html::css::UsedColorScheme) -> ImageSnapshot {
        if !self.synchronize_node(document, node, base) {
            return ImageSnapshot {
                complete: true,
                ..ImageSnapshot::default()
            };
        }
        let requests = self.requests.borrow();
        let Some(request) = requests.get(&node) else {
            return ImageSnapshot {
                complete: true,
                ..ImageSnapshot::default()
            };
        };
        let complete = match request.selected_source.as_ref() {
            Some(None) => true,
            Some(Some(source)) if source.is_empty() => true,
            Some(Some(_)) => {
                request.pending.is_none()
                    && matches!(
                        request.current.state,
                        RequestState::Available | RequestState::Broken
                    )
            }
            None => true,
        };
        let intrinsic=request.current.image.as_ref().and_then(|image|self.resolver.borrow().as_ref().and_then(|resolver|resolver.node_image_intrinsic_size(node,image,scheme))).or(request.current.intrinsic);
        let dimensions=request.current.image.as_ref().map(|image|intrinsic.unwrap_or(lumen_html::object::IntrinsicSize{width:Some(image.width as f32),height:Some(image.height as f32),ratio:None}).dimensions_at_density((300.0,150.0),request.current.density));
        ImageSnapshot{complete,current_src:request.current.current_src.clone(),natural_width:dimensions.map_or(0,|size|natural_dimension(size.0)),natural_height:dimensions.map_or(0,|size|natural_dimension(size.1)),natural_size:dimensions}
    }

    pub(crate) fn canvas_image_state(&self, node: NodeId) -> CanvasImageState {
        let requests = self.requests.borrow();
        let Some(request) = requests.get(&node) else {
            return CanvasImageState::Unavailable;
        };
        match request.current.state {
            RequestState::Broken => CanvasImageState::Broken,
            RequestState::Available => request
                .current
                .image
                .as_ref()
                .filter(|image| image.is_valid() && image.width != 0 && image.height != 0)
                .cloned()
                .map_or(CanvasImageState::Unavailable, |image| {
                    CanvasImageState::Available(image, request.current.origin_clean)
                }),
            RequestState::Unavailable | RequestState::Loading => CanvasImageState::Unavailable,
        }
    }

    fn refresh_discovery(&self,document:&Document,base:&str) {
        let mut discovery = self.discovery.borrow_mut();
        let discovery = &mut *discovery;
        let key = (self.tree_generation.get(), document.node_count());
        if discovery.key != Some(key) || discovery.base != base {
            collect_image_nodes(document, &mut discovery.nodes);
            discovery.seen.clear();
            discovery.seen.extend(discovery.nodes.iter().copied());
            discovery.stale.clear();
            for &node in self.requests.borrow().keys() {
                if is_html_image(document, node) {
                    if discovery.seen.insert(node) {
                        discovery.nodes.push(node);
                    }
                } else {
                    discovery.stale.push(node);
                }
            }
            if !discovery.stale.is_empty() {
                let mut requests = self.requests.borrow_mut();
                for node in discovery.stale.drain(..) {
                    requests.remove(&node);
                }
            }
            let base_changed=discovery.base!=base;
            let mut requests=self.requests.borrow_mut();
            for &node in &discovery.nodes {
                let request=requests.entry(node).or_default();
                // Rediscovery invalidates the observation proof, without
                // turning unrelated tree changes into new load events.
                request.observed_version=None;
                if base_changed && request.selected_source.is_some(){request.selection_dirty=true;}
            }
            self.has_dirty.set(true);
            discovery.key = Some(key);
            discovery.base.clear();
            discovery.base.push_str(base);
        }
    }

    /// A fresh frame is needed only for actual auto-size consumers after an
    /// input epoch change. Discover first, so a static first image is measured
    /// before its first source is selected, using the existing discovery cache.
    pub(crate) fn needs_auto_layout(&self,session:&lumen_html::session::RenderSession,base:&str)->bool {
        self.refresh_discovery(session.document(),base);
        let epoch=(session.document().version(),session.frame_id(),self.requests.borrow().len(),self.tree_generation.get());
        if self.auto_inputs_epoch.get()==Some(epoch)
            &&self.selection_environment.get().is_some_and(|(environment,_)|environment==session.media_environment()){return false;}
        self.requests.borrow().keys().any(|&node|session.document().is_connected_element(node)
            &&lumen_html::responsive_images::allows_auto_sizes(session.document(),node))
    }

    pub(crate) fn queue_completions(
        &self,
        document: &Document,
        base: &str,
    ) -> Vec<QueuedImageEvent> {
        self.refresh_discovery(document,base);
        self.synchronize_dirty(document,base);

        let resolver = self.resolver.borrow().clone();
        let resolver_generation = resolver.as_ref().map_or(0, |resolver| resolver.generation());
        let scan = self.needs_scan.get()
            || (self.loading_remaining.get()
                && (resolver_generation == 0
                    || self.polled_generation.get() != Some(resolver_generation)));
        if !scan {
            return Vec::new();
        }
        self.needs_scan.set(false);
        self.polled_generation.set(Some(resolver_generation));
        let mut still_loading = false;

        let mut queued = Vec::new();
        let mut discovery = self.discovery.borrow_mut();
        discovery.pending.clear();
        for (&node, request) in self.requests.borrow_mut().iter_mut() {
            if let Some(kind) = request.ready_event.take() {
                queued.push(QueuedImageEvent {
                    node,
                    generation: request.selection_generation,
                    kind,
                });
            }
            let selected = request
                .pending
                .as_ref()
                .filter(|image| image.state == RequestState::Loading)
                .or_else(|| {
                    (request.current.state == RequestState::Loading).then_some(&request.current)
                });
            if let Some(selected) = selected {
                discovery.pending.push(PendingLoad {
                    node,
                    generation: selected.generation,
                    current_src: selected.current_src.clone(),
                    base: request.base.clone(),
                    force_error: selected.force_error,
                    response_policy:selected.response_policy.clone(),
                });
            }
        }
        for PendingLoad {
            node,
            generation,
            current_src,
            base,
            force_error,
            response_policy,
        } in discovery.pending.drain(..)
        {
            // An empty src is a broken request, not a URL for the base document.
            // Never let a catch-all resolver turn it into a successful image.
            let state = if force_error {
                ImageState::Failed
            } else {
                resolver.as_ref().map_or(ImageState::Failed, |resolver| {
                    resolver
                        .resolve_node_from(node, &base, &current_src)
                        .unwrap_or_else(|| resolver.resolve_from(&base, &current_src))
                })
            };
            let state=if !force_error && !matches!(state,ImageState::Pending){
                let metadata=resolver.as_ref().and_then(|resolver|resolver.response_metadata(node,&base,&current_src));
                if metadata.zip(response_policy).is_some_and(|(metadata,policy)|self.record_policy_decision(policy(&current_src,&metadata.final_url,metadata.redirect_count))){ImageState::Failed}else{state}
            }else{state};
            let (image, kind) = match state {
                ImageState::Ready(image) if image.is_valid() => (Some(image), ImageEventKind::Load),
                ImageState::Pending => {
                    still_loading = true;
                    continue;
                }
                ImageState::Ready(_) | ImageState::Failed => (None, ImageEventKind::Error),
            };
            let intrinsic=image.as_ref().and_then(|image|resolver.as_ref().and_then(|resolver|resolver.node_image_intrinsic_size(node,image,lumen_html::css::UsedColorScheme::Light)));
            let origin_clean = image.is_some() && resolver
                .as_ref()
                .is_some_and(|resolver| resolver.node_origin_clean(node, &base, &current_src));
            let mut requests = self.requests.borrow_mut();
            let Some(request) = requests.get_mut(&node) else {
                continue;
            };
            let completed = if request
                .pending
                .as_ref()
                .is_some_and(|pending| pending.generation == generation)
            {
                let mut completed = request.pending.take().expect("pending request checked");
                complete_request(&mut completed, image, intrinsic, kind, origin_clean);
                request.current = completed;
                true
            } else if request.current.generation == generation
                && request.current.state == RequestState::Loading
            {
                complete_request(&mut request.current, image, intrinsic, kind, origin_clean);
                true
            } else {
                false
            };
            if completed {
                if request.environment_dirty {self.has_dirty.set(true);self.mark_scan_needed();}
                self.stage_current_image(request, node);
                queued.push(QueuedImageEvent {
                    node,
                    generation: request.selection_generation,
                    kind,
                });
            }
        }
        self.loading_remaining.set(still_loading);
        queued
    }

    fn mark_scan_needed(&self) {
        self.needs_scan.set(true);
        self.loading_remaining.set(true);
    }

    pub(crate) fn is_loading(&self, node: NodeId) -> bool {
        self.requests.borrow().get(&node).is_some_and(|request| {
            request.current.state == RequestState::Loading
                || request
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.state == RequestState::Loading)
        })
    }

    /// Loading nodes absent from `known`, collected into a reused buffer.
    pub(crate) fn take_unknown_pending<V>(&self, known: &HashMap<NodeId, V>) -> Vec<NodeId> {
        let mut out = std::mem::take(&mut *self.pending_scratch.borrow_mut());
        out.clear();
        if self.loading_remaining.get() {
            out.extend(self.requests.borrow().iter().filter_map(|(&node, request)| {
                (!known.contains_key(&node)
                    && (request.current.state == RequestState::Loading
                        || request
                            .pending
                            .as_ref()
                            .is_some_and(|pending| pending.state == RequestState::Loading)))
                .then_some(node)
            }));
        }
        out
    }

    pub(crate) fn return_pending_scratch(&self, scratch: Vec<NodeId>) {
        *self.pending_scratch.borrow_mut() = scratch;
    }

    pub(crate) fn pending_nodes(&self) -> Vec<NodeId> {
        self.requests
            .borrow()
            .iter()
            .filter_map(|(&node, request)| {
                (request.current.state == RequestState::Loading
                    || request
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.state == RequestState::Loading))
                .then_some(node)
            })
            .collect()
    }

    pub(crate) fn completion_is_current(&self, node: NodeId, generation: u64) -> bool {
        self.requests.borrow().get(&node).is_some_and(|request| {
            request.selection_generation == generation
                && request.current.event_queued
                && matches!(
                    request.current.state,
                    RequestState::Available | RequestState::Broken
                )
        })
    }

    pub(crate) fn adopt_nodes_into(&self, target: &Self, mapping: &[(NodeId, NodeId)]) {
        let mut source = self.requests.borrow_mut();
        let mut destination = target.requests.borrow_mut();
        for &(old, new) in mapping {
            source.remove(&old);
            destination.remove(&new);
        }
        self.bump_tree_generation();
        target.bump_tree_generation();
    }

    fn bump_tree_generation(&self) {
        self.tree_generation
            .set(self.tree_generation.get().wrapping_add(1));
    }

    fn allocate_generation(&self) -> u64 {
        let next = self.next_generation.get().wrapping_add(1).max(1);
        self.next_generation.set(next);
        next
    }
}

fn natural_dimension(value:f64)->u32 {
    if !value.is_finite()||value==0.0{return 0;}
    value.trunc().rem_euclid(4294967296.0) as u32
}

fn complete_request(
    request: &mut ImageRequest,
    image: Option<Arc<ImageData>>,
    intrinsic:Option<lumen_html::object::IntrinsicSize>,
    kind: ImageEventKind,
    origin_clean: bool,
) {
    request.image = image;
    request.intrinsic=intrinsic;
    request.state = if kind == ImageEventKind::Load {
        RequestState::Available
    } else {
        RequestState::Broken
    };
    request.force_error = false;
    request.event_queued = true;
    request.origin_clean = origin_clean;
    request.response_policy=None;
}

fn same_image(left: Option<&Arc<ImageData>>, right: Option<&Arc<ImageData>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn collect_image_nodes(document: &Document, images: &mut Vec<NodeId>) {
    images.clear();
    let root = document.root();
    let mut current = Some(root);
    while let Some(node) = current {
        if is_html_image(document, node) {
            images.push(node);
        }
        current = lumen_html::selector::next_shadow_including_descendant(document, root, node)
            .ok()
            .flatten();
    }
}

fn image_source(document: &Document, node: NodeId) -> Option<Option<String>> {
    match document.kind(node).ok()? {
        NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } if name == "img" => Some(document.get_attribute_ns(node, None, "src").ok().flatten()),
        _ => None,
    }
}

fn image_crossorigin(document: &Document, node: NodeId) -> Option<bool> {
    let NodeKind::Element {
        namespace: Namespace::Html,
        name,
        ..
    } = document.kind(node).ok()?
    else {
        return None;
    };
    if name != "img" {
        return None;
    }
    document
        .get_attribute_ns(node, None, "crossorigin")
        .ok()
        .flatten()
        .map(|value| crossorigin_state(Some(value.as_str())).unwrap_or(false))
}

fn crossorigin_state(value: Option<&str>) -> Option<bool> {
    value.map(|value| value.eq_ignore_ascii_case("use-credentials"))
}

fn is_html_image(document: &Document, node: NodeId) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "img"
    )
}

pub(crate) fn resolved_url(source: &str, base: &str) -> String {
    lumen_common::url::parse(source, Some(base))
        .map(|url| url.href())
        .unwrap_or_default()
}

#[cfg(test)]
mod responsive_request_tests {
    use super::*;
    #[test]
    fn specification_canvas_source_dimensions_precede_webidl_projection() {
        let document=lumen_html::html::parse("<img src='actual.png'>",16).unwrap();
        let node=lumen_html::selector::query_selector(&document,document.root(),"img").unwrap().unwrap();
        let loader=ImageLoader::default();let base="https://images.test/page.html";
        loader.snapshot(&document,node,base,lumen_html::css::UsedColorScheme::Light);
        {
            let mut requests=loader.requests.borrow_mut();let request=requests.get_mut(&node).unwrap();
            request.current.state=RequestState::Available;request.current.density=4.0;
            request.current.image=Some(Arc::new(ImageData{width:2,height:2,pixels:vec![0,128,0,255].repeat(4)}));
        }
        let snapshot=loader.snapshot(&document,node,base,lumen_html::css::UsedColorScheme::Light);
        assert_eq!(snapshot.natural_size,Some((0.5,0.5)));
        assert_eq!((snapshot.natural_width,snapshot.natural_height),(0,0));
    }

    #[test]
    fn specification_same_pending_url_preserves_prepared_density() {
        let loader=ImageLoader::default();let document=Document::new(8);let node=document.root();
        let mut request=Request::default();
        request.current.state=RequestState::Available;
        request.current.current_src=String::from("https://images.test/old.png");
        loader.select_source(&mut request,node,Some(String::from("next.png")),2.0,None,"https://images.test/page.html");
        let generation=request.pending.as_ref().unwrap().generation;
        loader.select_source(&mut request,node,Some(String::from("next.png")),4.0,None,"https://images.test/page.html");
        let pending=request.pending.as_ref().unwrap();
        assert_eq!(pending.generation,generation);
        assert_eq!(pending.density,2.0);
        assert_eq!(request.selected_density,Some(4.0));
        assert_eq!(request.current.density,1.0);
    }
    #[test]
    fn specification_first_discovered_auto_image_uses_fresh_frame_and_warm_epoch() {
        let document=lumen_html::html::parse("<!doctype html><body><img loading=lazy sizes=auto srcset='actual.png 400w' style='width:200px;height:100px;border:10px solid;padding:10px'></body>",32).unwrap();
        let mut session=lumen_html::session::RenderSession::new(document);
        let loader=ImageLoader::default();let base="https://images.test/page.html";
        assert!(loader.needs_auto_layout(&session,base));
        assert_eq!(loader.requests.borrow().len(),1,"discovery precedes the first selection");
        session.display_list(600,400,crate::canvas::canvas_fallback_fonts()).unwrap();
        loader.configure_selection(&mut session,1.0,base);
        loader.synchronize_dirty(session.document(),base);
        let node=*loader.requests.borrow().keys().next().unwrap();
        assert_eq!(loader.requests.borrow()[&node].auto_width,Some(200.0),"source size uses content width, excluding the actual box edges");
        assert_eq!(loader.requests.borrow()[&node].current.density,2.0);
        assert!(!loader.needs_auto_layout(&session,base),"warm owner inputs reuse their frame epoch");
        session.document_mut().set_attribute(node,"style","width:100px;height:100px").unwrap();
        assert!(loader.needs_auto_layout(&session,base),"style mutation requires a fresh frame");
        session.display_list(600,400,crate::canvas::canvas_fallback_fonts()).unwrap();
        loader.configure_selection(&mut session,1.0,base);
        assert_eq!(loader.requests.borrow()[&node].auto_width,Some(100.0));
    }

}
