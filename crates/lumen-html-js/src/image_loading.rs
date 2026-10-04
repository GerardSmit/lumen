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
    pub(crate) origin_clean: bool,
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

#[derive(Default)]
struct ImageRequest {
    current_src: String,
    generation: u64,
    state: RequestState,
    image: Option<Arc<ImageData>>,
    force_error: bool,
    event_queued: bool,
    origin_clean: bool,
}

impl ImageRequest {
    fn loading(current_src: String, generation: u64, force_error: bool) -> Self {
        Self {
            current_src,
            generation,
            state: RequestState::Loading,
            image: None,
            force_error,
            event_queued: false,
            origin_clean: false,
        }
    }
}

#[derive(Default)]
struct Request {
    // The outer option records whether DOM attributes have been observed yet;
    // its inner option distinguishes an omitted src from src="".
    selected_source: Option<Option<String>>,
    selected_crossorigin: Option<Option<bool>>,
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
}

#[derive(Default)]
pub(crate) struct ImageLoader {
    resolver: RefCell<Option<Rc<dyn ImageResolver>>>,
    requests: RefCell<HashMap<NodeId, Request>>,
    bitmap_updates: RefCell<HashMap<NodeId, Option<Arc<ImageData>>>>,
    next_generation: Cell<u64>,
}

impl ImageLoader {
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
                } else if name.eq_ignore_ascii_case("crossorigin")
                    && crossorigin_state(old_value.as_deref())
                        != image_crossorigin(document, mutation.target)
                {
                    // crossorigin is relevant when its parsed state changes,
                    // not merely when its raw spelling is different.
                    self.note_source_change(document, mutation.target);
                }
            }
            _ => {}
        }
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
        // Invalidate any completion task already queued for the previous
        // selection immediately. The next synchronization either preserves
        // its in-flight request or stages a fresh cached completion.
        request.selection_generation = self.allocate_generation();
    }

    fn synchronize_node(&self, document: &Document, node: NodeId, base: &str) -> bool {
        let Some(source) = image_source(document, node) else {
            return false;
        };
        let crossorigin = image_crossorigin(document, node);
        let mut requests = self.requests.borrow_mut();
        let request = requests.entry(node).or_default();
        if !request.selection_dirty
            && request
                .selected_source
                .as_ref()
                .is_some_and(|selected| selected == &source)
            && request.selected_crossorigin == Some(crossorigin)
        {
            return true;
        }
        self.select_source(request, node, source, crossorigin, base);
        true
    }

    fn select_source(
        &self,
        request: &mut Request,
        node: NodeId,
        source: Option<String>,
        crossorigin: Option<bool>,
        base: &str,
    ) {
        request.selection_dirty = false;
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
            request.selected_source = Some(Some(source));
            request.selected_crossorigin = Some(crossorigin);
            request.base = base.to_owned();
            request.pending = None;
            self.stage_current_image(request, node);
            return;
        }
        if let Some(pending) = request.pending.as_ref() {
            if !cors_mode_changed
                && pending.current_src == current_src
                && pending.force_error == force_error
            {
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

        request.selection_generation = self.allocate_generation();
        request.ready_event = None;
        request.selected_source = Some(Some(source.clone()));
        request.selected_crossorigin = Some(crossorigin);
        request.base = base.to_owned();
        let next = ImageRequest::loading(current_src, self.allocate_generation(), force_error);
        if request.current.state == RequestState::Available {
            request.pending = Some(next);
        } else {
            request.current = next;
            request.pending = None;
        }
        self.stage_current_image(request, node);
    }

    fn stage_current_image(&self, request: &mut Request, node: NodeId) {
        if same_image(
            request.published_image.as_ref(),
            request.current.image.as_ref(),
        ) {
            return;
        }
        let image = request.current.image.clone();
        request.published_image = image.clone();
        self.bitmap_updates.borrow_mut().insert(node, image);
    }

    pub(crate) fn take_bitmap_updates(&self) -> Vec<(NodeId, Option<Arc<ImageData>>)> {
        self.bitmap_updates.borrow_mut().drain().collect()
    }

    pub(crate) fn snapshot(&self, document: &Document, node: NodeId, base: &str) -> ImageSnapshot {
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
        ImageSnapshot {
            complete,
            current_src: request.current.current_src.clone(),
            natural_width: request
                .current
                .image
                .as_ref()
                .map_or(0, |image| image.width),
            natural_height: request
                .current
                .image
                .as_ref()
                .map_or(0, |image| image.height),
            origin_clean: request.current.origin_clean,
        }
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

    pub(crate) fn queue_completions(
        &self,
        document: &Document,
        base: &str,
    ) -> Vec<QueuedImageEvent> {
        let mut nodes = active_image_nodes(document);
        let mut seen: HashSet<_> = nodes.iter().copied().collect();
        let existing = self.requests.borrow().keys().copied().collect::<Vec<_>>();
        for node in existing {
            if is_html_image(document, node) {
                if seen.insert(node) {
                    nodes.push(node);
                }
            } else {
                self.requests.borrow_mut().remove(&node);
            }
        }

        for node in nodes.iter().copied() {
            self.synchronize_node(document, node, base);
        }

        let mut queued = {
            let mut requests = self.requests.borrow_mut();
            requests
                .iter_mut()
                .filter_map(|(&node, request)| {
                    let kind = request.ready_event.take()?;
                    Some(QueuedImageEvent {
                        node,
                        generation: request.selection_generation,
                        kind,
                    })
                })
                .collect::<Vec<_>>()
        };

        let pending = self
            .requests
            .borrow()
            .iter()
            .filter_map(|(node, request)| {
                let selected = request
                    .pending
                    .as_ref()
                    .filter(|image| image.state == RequestState::Loading)
                    .or_else(|| {
                        (request.current.state == RequestState::Loading).then_some(&request.current)
                    })?;
                Some((
                    *node,
                    selected.generation,
                    selected.current_src.clone(),
                    selected.force_error,
                ))
            })
            .collect::<Vec<_>>();
        let resolver = self.resolver.borrow().clone();
        for (node, generation, current_src, force_error) in pending {
            // An empty src is a broken request, not a URL for the base document.
            // Never let a catch-all resolver turn it into a successful image.
            let state = if force_error {
                ImageState::Failed
            } else {
                let base = self
                    .requests
                    .borrow()
                    .get(&node)
                    .map_or_else(String::new, |request| request.base.clone());
                resolver.as_ref().map_or(ImageState::Failed, |resolver| {
                    resolver
                        .resolve_node_from(node, &base, &current_src)
                        .unwrap_or_else(|| resolver.resolve_from(&base, &current_src))
                })
            };
            let (image, kind) = match state {
                ImageState::Ready(image) if image.is_valid() => (Some(image), ImageEventKind::Load),
                ImageState::Pending => continue,
                ImageState::Ready(_) | ImageState::Failed => (None, ImageEventKind::Error),
            };
            let base = self
                .requests
                .borrow()
                .get(&node)
                .map_or_else(String::new, |request| request.base.clone());
            let origin_clean = resolver
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
                complete_request(&mut completed, image, kind, origin_clean);
                request.current = completed;
                true
            } else if request.current.generation == generation
                && request.current.state == RequestState::Loading
            {
                complete_request(&mut request.current, image, kind, origin_clean);
                true
            } else {
                false
            };
            if completed {
                self.stage_current_image(request, node);
                queued.push(QueuedImageEvent {
                    node,
                    generation: request.selection_generation,
                    kind,
                });
            }
        }
        queued
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
    }

    fn allocate_generation(&self) -> u64 {
        let next = self.next_generation.get().wrapping_add(1).max(1);
        self.next_generation.set(next);
        next
    }
}

fn complete_request(
    request: &mut ImageRequest,
    image: Option<Arc<ImageData>>,
    kind: ImageEventKind,
    origin_clean: bool,
) {
    request.image = image;
    request.state = if kind == ImageEventKind::Load {
        RequestState::Available
    } else {
        RequestState::Broken
    };
    request.force_error = false;
    request.event_queued = true;
    request.origin_clean = origin_clean;
}

fn same_image(left: Option<&Arc<ImageData>>, right: Option<&Arc<ImageData>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        _ => false,
    }
}

fn active_image_nodes(document: &Document) -> Vec<NodeId> {
    let mut images = Vec::new();
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
    images
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
