//! DOM objects for one Lumen realm, backed by the shared Rust render session.
use lumen::embed::{Ctx, JsObject, OpError, OpResult, Value, WeakValue};
use lumen_html::selector::next_descendant;
use lumen_html::{html, selector, session::RenderSession, Error, Namespace, NodeId, NodeKind};
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
};
mod error_reporting;
mod event_content_handlers;
mod events;
use events::{DomEvent, DomEventTarget, TargetData};
mod attributes;
mod collections;
mod dataset;
mod ui_events;
use collections::{
    html_space_tokens, DescendantFilter, DomCollectionIterator, DomHtmlCollection,
    DomHtmlFormControlsCollection, DomNodeList, DomTokenList,
};
mod style;
use style::DomStyle;
pub(crate) mod animations;
mod browser_services;
mod browsing_context;
mod realm_services;
pub use browser_services::{ClipboardHost, ClipboardOperation, ClipboardPermission};
pub use browsing_context::{
    FrameCommitResult, FrameContext, FrameInstallError, FrameNavigationRequest, FrameSource,
    FrameUnsupportedReason, Origin, PreparedFrameResponse, WeakFrameIdentity,
};
mod presentation;
pub use presentation::PresentationHost;
mod notifications;
pub mod object_urls;
pub use notifications::{
    NotificationHost, NotificationHostEvent, NotificationPayload, NotificationPermission,
    NotificationPermissionRequest,
};
mod canvas;
pub use canvas::{
    prune_dead_thread_handles as prune_dead_canvas_handles, set_gpu_canvas_context_factory,
    set_webgl_canvas_context_factory, CanvasGpuTarget,
};
mod cookies;
pub(crate) mod dialog_popover;
mod cssom;
mod custom_elements;
pub use cookies::CookieHost;
mod document_utilities;
mod editing;
mod editing_history;
mod font_loading;
#[cfg(test)]
#[path = "forms_batch68_tests.rs"]
mod forms_batch68_tests;
#[cfg(test)]
#[path = "forms_batch69_tests.rs"]
mod forms_batch69_tests;
#[cfg(test)]
#[path = "forms_batch70_tests.rs"]
mod forms_batch70_tests;
#[cfg(test)]
#[path = "forms_batch71_tests.rs"]
mod forms_batch71_tests;
mod form_data_bridge;
pub mod forms;
mod geometry;
mod html_interfaces;
mod hyperlinks;
mod labels;
mod image_loading;
pub mod keyboard_automation;
mod media;
mod media_capture;
mod option_factory;
pub(crate) mod scrolling;
pub mod testdriver;
mod webaudio;
mod webrtc;
pub use font_loading::{FontFaceStatus, FontResourceLoader, ManualFontFace};
pub use media::{MediaSnapshot, ReadyState as MediaReadyState, VideoFrameSnapshot};
pub use media_capture::{
    MediaCapturePermission, MediaCaptureVideoSource, MediaDeviceDescription, MediaDeviceKind,
    MediaDevicesHost,
};
pub use webaudio::{
    AudioTarget, BufferSnapshot, ContextSnapshot, ContextState, GainSnapshot, OscillatorSnapshot,
    ParamEvent, Waveform,
};
pub use webrtc::{WebRtcHost, WebRtcHostEvent};
mod script_loading;
#[derive(Default)]
struct ScriptCapabilities {
    modules: Cell<bool>,
}
pub use script_loading::{
    DeclaredScriptType as ScriptType, ScriptDescriptor, UnhandledScriptActivation,
    UnhandledScriptReason,
};
mod jsx;
pub mod layout_observers;
mod observers;
mod range;
mod reactive;
mod rejection_delivery;
pub use rejection_delivery::{
    admit_document_rejection_events, group_rejection_tasks, queue_document_rejection_events,
    register_document_rejection_sink, run_rejection_task, BrowserRejectionBatch,
    BrowserRejectionDelivery, BrowserRejectionEvent, BrowserRejectionPolicy, BrowserRejectionSink,
    BrowserRejectionTask,
};
pub mod scheduling;
mod shadow;
mod templates;
mod window_globals;
use editing_history::EditingState;
use shadow::{DomShadowRoot, DomSlotElement};

/// Set the real ICE/DTLS/SCTP transport for this engine before page scripts
/// create an RTCPeerConnection. Passing `None` makes construction reject.
pub fn set_webrtc_host(ctx: &mut Ctx, host: Option<Rc<dyn WebRtcHost>>) {
    webrtc::set_host(ctx, host);
}

pub fn pump_webrtc(ctx: &mut Ctx) -> lumen::embed::OpResult<()> {
    webrtc::pump(ctx)
}

/// Install the document-independent CSS Typed OM interfaces in a browser
/// worker realm. Window installation uses the same implementation through
/// the full CSSOM installer.
pub fn install_css_typed_om(ctx: &mut Ctx) -> OpResult<()> {
    ctx.set_none_is_null(true);
    cssom::install_typed_numeric(ctx)
}

/// Install the OffscreenCanvas interfaces and worker-safe canvas helpers in a
/// browser worker realm without exposing Window's CanvasRenderingContext2D.
pub fn install_worker_canvas(ctx: &mut Ctx) -> OpResult<()> {
    ctx.set_none_is_null(true);
    canvas::install_worker(ctx)
}

#[inline(always)]
fn enter_html_allocation_category() -> lumen::memstats::CatGuard {
    lumen::memstats::enter(lumen::memstats::Cat::Html)
}

/// Source facades to bundle with an app; their implementation is installed natively.
pub fn module_source(specifier: &str) -> Option<&'static str> {
    match specifier {
        "lumen" => Some(
            "export const {signal,effect,memo,batch,untrack,createRoot,onCleanup,errorBoundary,template,instantiate,nodeAt,bindText,bindAttribute,bindChild,For,Show}=globalThis.__lumen;",
        ),
        "lumen/jsx-runtime" | "lumen/jsx-dev-runtime" => {
            Some("export const {jsx,jsxs,jsxDEV,Fragment}=globalThis.__lumen;")
        }
        _ => None,
    }
}

#[derive(Debug)]
pub enum InstallError {
    Parse(html::ParseError),
    XmlParse(lumen_html::xml::ParseError),
    Global,
}

/// XML document MIME types with an active browsing context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum XmlDocumentType {
    ApplicationXml,
    TextXml,
    Xhtml,
    Svg,
}

impl XmlDocumentType {
    pub const fn content_type(self) -> &'static str {
        match self {
            Self::ApplicationXml => "application/xml",
            Self::TextXml => "text/xml",
            Self::Xhtml => "application/xhtml+xml",
            Self::Svg => "image/svg+xml",
        }
    }
}

const VALUE_WRITE_JOURNAL_LEN: usize = 64;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DocumentReadyState {
    Loading,
    Interactive,
    Complete,
}

impl DocumentReadyState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Interactive => "interactive",
            Self::Complete => "complete",
        }
    }
}

struct ValueWriteJournal {
    entries: [Option<(u64, NodeId)>; VALUE_WRITE_JOURNAL_LEN],
}

/// The one base element currently governing this document, together with the
/// URL frozen when that element became first (or its href changed while first).
/// Keeping just the active element avoids retaining state for every parsed
/// `<base>` in large documents.
#[derive(Default)]
struct DocumentBaseUrlCache {
    initialized: bool,
    first_base: Option<NodeId>,
    frozen_url: Option<String>,
}

fn is_html_base(document: &lumen_html::Document, node: NodeId) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if lumen_html::xml::split_qname(name.as_str())
            .is_some_and(|(_, local_name)| local_name == "base")
    )
}

fn is_html_iframe(document: &lumen_html::Document, node: NodeId) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if lumen_html::xml::split_qname(name.as_str())
            .is_some_and(|(_, local_name)| local_name == "iframe")
    )
}

fn connected_interaction_element(document: &lumen_html::Document, node: NodeId) -> bool {
    matches!(document.kind(node), Ok(NodeKind::Element { .. }))
        && script_loading::is_connected(document, node)
}

fn is_frame_navigation_attribute(name: &str, is_html_document: bool) -> bool {
    ["src", "srcdoc", "sandbox"].into_iter().any(|attribute| {
        if is_html_document {
            name.eq_ignore_ascii_case(attribute)
        } else {
            name == attribute
        }
    })
}

fn html_base_href(
    document: &lumen_html::Document,
    node: NodeId,
    is_html_document: bool,
) -> Option<String> {
    let NodeKind::Element {
        namespace,
        attributes,
        ..
    } = document.kind(node).ok()?
    else {
        return None;
    };
    if *namespace != Namespace::Html {
        return None;
    }
    let fold_attribute_case = is_html_document && *namespace == Namespace::Html;
    attributes
        .iter()
        .enumerate()
        .find_map(|(index, (name, value))| {
            let matches_name = if fold_attribute_case {
                name.as_str().eq_ignore_ascii_case("href")
            } else {
                name.as_str() == "href"
            };
            (matches_name && document.attribute_namespace_uri_at(node, index).is_none())
                .then(|| value.clone())
        })
}

/// Find the first HTML base with href in tree order. The walk is allocation-free
/// and deliberately ignores template contents and shadow trees.
fn first_html_base_with_href(
    document: &lumen_html::Document,
    boundary: NodeId,
    include_boundary: bool,
    is_html_document: bool,
) -> Option<(NodeId, String)> {
    let mut current = if include_boundary {
        Some(boundary)
    } else {
        document.first_child(boundary).ok().flatten()
    };
    while let Some(node) = current {
        if is_html_base(document, node) {
            if let Some(href) = html_base_href(document, node, is_html_document) {
                return Some((node, href));
            }
        }
        current = lumen_html::selector::next_descendant(document, boundary, node)
            .ok()
            .flatten();
    }
    None
}

fn subtree_has_html_base_with_href(
    document: &lumen_html::Document,
    root: NodeId,
    is_html_document: bool,
) -> bool {
    first_html_base_with_href(document, root, true, is_html_document).is_some()
}

fn mutation_subtree_has_base(
    document: &lumen_html::Document,
    nodes: impl IntoIterator<Item = NodeId>,
    is_html_document: bool,
) -> bool {
    nodes
        .into_iter()
        .any(|node| subtree_has_html_base_with_href(document, node, is_html_document))
}

fn mutation_contains_node(
    document: &lumen_html::Document,
    mutation: &lumen_html::observe::ObservedMutation,
    target: NodeId,
) -> bool {
    let contains = |root: NodeId| {
        let mut current = Some(target);
        while let Some(node) = current {
            if node == root {
                return true;
            }
            current = document.parent(node).ok().flatten();
        }
        false
    };
    match &mutation.kind {
        lumen_html::observe::ObservedKind::ChildList { added, removed, .. } => added
            .iter()
            .chain(removed.iter())
            .any(|root| contains(*root)),
        lumen_html::observe::ObservedKind::ChildListMany { added, removed } => {
            added.iter().chain(removed).any(|root| contains(*root))
        }
        lumen_html::observe::ObservedKind::Attribute { .. }
        | lumen_html::observe::ObservedKind::CharacterData { .. }
        | lumen_html::observe::ObservedKind::SlotAssignment => false,
    }
}

fn attribute_name_is_href(name: &str, is_html_document: bool) -> bool {
    if is_html_document {
        name.eq_ignore_ascii_case("href")
    } else {
        name == "href"
    }
}

fn freeze_base_href(href: &str, fallback: &str) -> String {
    let Some(url) = lumen_common::url::parse(href, Some(fallback)).ok() else {
        return fallback.to_owned();
    };
    if matches!(url.scheme.as_str(), "data" | "javascript") {
        fallback.to_owned()
    } else {
        url.href()
    }
}

impl Default for ValueWriteJournal {
    fn default() -> Self {
        Self {
            entries: [None; VALUE_WRITE_JOURNAL_LEN],
        }
    }
}

impl ValueWriteJournal {
    fn record(&mut self, sequence: u64, node: NodeId) {
        let index = (sequence & (VALUE_WRITE_JOURNAL_LEN as u64 - 1)) as usize;
        self.entries[index] = Some((sequence, node));
    }

    fn changed_since(&self, node: NodeId, epoch: u64, current: u64) -> bool {
        let count = current.saturating_sub(epoch);
        if count == 0 {
            return false;
        }
        if count > VALUE_WRITE_JOURNAL_LEN as u64 {
            // The bounded journal rolled over during a reentrant listener. Treat
            // the history as stale rather than risk applying over an unseen write.
            return true;
        }
        ((epoch + 1)..=current).any(|sequence| {
            let index = (sequence & (VALUE_WRITE_JOURNAL_LEN as u64 - 1)) as usize;
            self.entries[index] == Some((sequence, node))
        })
    }
}

/// Identity metadata can outlive an arena when objects from an old Window,
/// such as DOMParser, remain reachable after navigation.
#[derive(Default)]
struct DocumentIdentity {
    origin: RefCell<Option<browsing_context::Origin>>,
    url: RefCell<Option<String>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DocumentInterface {
    Document,
    XmlDocument,
}

pub struct DomRealm {
    details_controller: RefCell<std::rc::Weak<dialog_popover::DetailsController>>,
    /// Last rendering-opportunity sample, shared by all document timelines.
    /// Keep this on the document so foreign-realm getters use its own clock.
    timeline_sample: Cell<f64>,
    media_capture: media_capture::RealmMediaCapture,
    browser_services: browser_services::RealmBrowserServices,
    /// The security origin associated with this Document, independent of its
    /// URL and browsing-context presence. Detached Documents snapshot the
    /// creating global's origin here.
    document_identity: Rc<DocumentIdentity>,
    about_base_url: RefCell<Option<String>>,
    document_base_url: RefCell<DocumentBaseUrlCache>,
    cookie_host: RefCell<Option<Rc<dyn CookieHost>>>,
    document_interface: DocumentInterface,
    content_type: String,
    document_encoding: Cell<&'static str>,
    is_html_document: bool,
    has_browsing_context: bool,
    file_picker_host: RefCell<Option<Rc<dyn Fn(forms::FilePickerRequest) -> Result<(), String>>>>,
    form_submission_host:
        RefCell<Option<Rc<dyn Fn(&mut Ctx, forms::FormSubmissionRequest) -> Result<(), String>>>>,
    canvases: canvas::CanvasRegistry,
    forms: Rc<RefCell<forms::FormState>>,
    ranges: Rc<range::RangeRegistry>,
    iterators: Rc<document_utilities::IteratorRegistry>,
    tree_walkers: Rc<document_utilities::TreeWalkerRegistry>,
    selection: RefCell<Option<Rc<range::SelectionData>>>,
    selection_wrapper: RefCell<Option<WeakValue>>,
    ready_state: Cell<DocumentReadyState>,
    current_script: Cell<Option<NodeId>>,
    document_parser: RefCell<Option<html::HtmlDocumentParser>>,
    parser_generation: Cell<u64>,
    scripts: RefCell<script_loading::ScriptLoader>,
    module_activations_enabled: Cell<bool>,
    script_capabilities: Rc<ScriptCapabilities>,
    classic_resource_activations_enabled: Cell<bool>,
    module_activations: RefCell<VecDeque<PendingScriptActivation>>,
    dataset_intrinsics: RefCell<Option<dataset::ProxyIntrinsics>>,
    layout_flusher: RefCell<Option<Rc<dyn Fn(&mut RenderSession) -> Result<(), String>>>>,
    font_loading: font_loading::FontLoading,
    images: image_loading::ImageLoader,
    pub(crate) media: RefCell<media::MediaController>,
    pub(crate) web_audio: RefCell<webaudio::WebAudioController>,
    image_request_roots: RefCell<HashMap<NodeId, ImageRequestRoot>>,
    mutation_sinks:
        RefCell<Vec<Rc<dyn Fn(&lumen_html::Document, &lumen_html::observe::ObservedMutation)>>>,
    session: Rc<RefCell<RenderSession>>,
    wrappers: RefCell<HashMap<NodeId, WeakValue>>,
    identity_trace_epoch: Cell<u64>,
    identity_trace_nodes: RefCell<HashSet<NodeId>>,
    adopted_nodes: RefCell<HashMap<NodeId, (std::rc::Weak<DomRealm>, NodeId)>>,
    document_wrapper: RefCell<Option<WeakValue>>,
    implementation_wrapper: RefCell<Option<WeakValue>>,
    detached: RefCell<Vec<NodeId>>,
    sweep_at: Cell<usize>,
    targets: RefCell<HashMap<NodeId, std::rc::Weak<TargetData>>>,
    retained_nodes: RefCell<HashMap<NodeId, usize>>,
    script_retentions:
        RefCell<HashMap<NodeId, Vec<std::rc::Weak<RefCell<CurrentScriptRetention>>>>>,
    window_target: RefCell<Option<Rc<TargetData>>>,
    window_wrapper: RefCell<Option<WeakValue>>,
    location_wrapper: RefCell<Option<WeakValue>>,
    browsing_context: RefCell<std::rc::Weak<browsing_context::BrowsingContext>>,
    frame_contexts: RefCell<HashMap<NodeId, std::rc::Weak<browsing_context::BrowsingContext>>>,
    pending_frame_contexts: RefCell<HashMap<NodeId, Rc<browsing_context::BrowsingContext>>>,
    pending_frame_realms: RefCell<Vec<lumen::embed::RealmHandle>>,
    pending_iframe_post_connections: RefCell<VecDeque<NodeId>>,
    focused: Cell<Option<NodeId>>,
    focus_visible: Cell<Option<NodeId>>,
    hover_target: Cell<Option<NodeId>>,
    active_targets: Cell<[Option<NodeId>; 2]>,
    keyboard_modality: Cell<bool>,
    selections: RefCell<HashMap<NodeId, (usize, usize, String)>>,
    editing: RefCell<EditingState>,
    programmatic_value_epoch: Cell<u64>,
    programmatic_value_writes: RefCell<ValueWriteJournal>,
}

impl DomRealm {
    pub(crate) fn set_document_origin(&self, origin: browsing_context::Origin) {
        *self.document_identity.origin.borrow_mut() = Some(origin);
    }

    pub(crate) fn document_origin(&self) -> Option<browsing_context::Origin> {
        self.document_identity.origin.borrow().clone()
    }

    /// Return this document's active child host realm. The root document has no
    /// child handle, and a replaced or detached frame document is not current.
    pub fn child_realm_handle(&self) -> OpResult<Option<lumen::embed::RealmHandle>> {
        if let Some(context) = self.browsing_context() {
            return browsing_context::active_child_realm_handle(&context, self);
        }
        if self.has_browsing_context {
            return Err(OpError::new(
                "InvalidStateError",
                "document browsing context is no longer available",
            ));
        }
        Ok(None)
    }

    /// Record that the embedder has started a script element. Parser-driven
    /// execution uses this before evaluation so insertion hooks cannot execute
    /// the same node a second time.
    pub fn mark_script_started(&self, node: NodeId) {
        self.scripts.borrow_mut().mark_started(node);
    }

    /// Record a parser script whose turn was considered but did not execute
    /// (for example, an empty script or a data block).
    pub fn mark_parser_script_prepared(&self, node: NodeId) {
        let suppressed = matches!(
            script_loading::prepare_kind(
                self.session.borrow().document(),
                node,
                self.is_html_document
            ),
            Some(script_loading::ScriptKind::SuppressedClassic)
        );
        if suppressed {
            self.scripts.borrow_mut().mark_started(node);
        } else {
            self.scripts.borrow_mut().mark_parser_prepared(node);
        }
    }

    /// Whether this script node has already been prepared and started.
    pub fn script_started(&self, node: NodeId) -> bool {
        self.scripts.borrow().already_started(node)
    }

    /// Whether a script's current `type` and `nomodule` attributes select a
    /// classic script. This shares the adapter's script MIME classification
    /// with host parser execution.
    pub fn script_is_classic(&self, node: NodeId) -> bool {
        let session = self.session.borrow();
        script_loading::is_script(session.document(), node)
            && script_loading::is_classic_type(session.document(), node, self.is_html_document)
    }

    /// Current scripts in DOM order, including XML-prefixed HTML and SVG script
    /// elements. External SVG resources use href or expanded-name XLink href.
    pub fn document_scripts(&self) -> Vec<ScriptDescriptor> {
        let session = self.session.borrow();
        let document = session.document();
        let states = self.scripts.borrow();
        script_loading::parser_scripts(document)
            .into_iter()
            .filter_map(|node| {
                script_loading::descriptor(
                    document,
                    node,
                    self.is_html_document,
                    states.force_async(node),
                    states.parser_inserted(node),
                )
            })
            .collect()
    }

    /// Resource-backed script preparations which the host has not executed.
    /// The host can reject these explicitly instead of inferring support from
    /// a JavaScript-side identity snapshot.
    pub fn unhandled_script_activations(&self) -> Vec<UnhandledScriptActivation> {
        self.scripts.borrow().unhandled()
    }

    /// Enable the host's resource-backed module activation queue. Hosts without
    /// a module loader retain the existing explicit unsupported diagnostics.
    pub fn enable_module_script_activations(&self) {
        self.module_activations_enabled.set(true);
    }

    /// Advertise module scripts only when this realm has an actual evaluator.
    /// Capturing resource activations alone does not provide module execution.
    pub fn set_module_script_support(&self, supported: bool) {
        self.script_capabilities.modules.set(supported);
    }

    /// Enable captured external classic preparations as well as modules for a
    /// host that implements their resource fetch and ordered execution queues.
    pub fn enable_resource_script_activations(&self) {
        self.enable_module_script_activations();
        self.classic_resource_activations_enabled.set(true);
    }

    /// Captured dynamic module preparations in activation order. Each lease
    /// retains its native node/listeners until the host drops it after dispatching
    /// the actual terminal load/error task, even if the element is detached.
    pub fn drain_script_activations(self: &Rc<Self>, ctx: &mut Ctx) -> Vec<ScriptActivation> {
        let pending: Vec<_> = self.module_activations.borrow_mut().drain(..).collect();
        pending
            .into_iter()
            .map(|pending| self.lease_script_activation(ctx, pending))
            .collect()
    }

    fn lease_script_activation(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        pending: PendingScriptActivation,
    ) -> ScriptActivation {
        let (owner, node) = self.resolve_adopted_node(pending.script.node);
        let wrapper = owner.wrap(ctx, node);
        ScriptActivation {
            script: pending.script,
            base_url: pending.base_url,
            _retention: pending.retention,
            _target: pending.target,
            _wrapper: wrapper,
            realm: self.clone(),
        }
    }

    /// Retain a parser-prepared script across asynchronous fetch/evaluation.
    /// Preparation captures the same native descriptor/base URL as dynamic
    /// activation and preserves detached targets until terminal event dispatch.
    pub fn retain_script_target(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
    ) -> OpResult<ScriptActivation> {
        let script = {
            let session = self.session.borrow();
            let scripts = self.scripts.borrow();
            script_loading::descriptor(
                session.document(),
                node,
                self.is_html_document,
                scripts.force_async(node),
                scripts.parser_inserted(node),
            )
        }
        .ok_or_else(|| OpError::type_error("script target required"))?;
        let pending = PendingScriptActivation {
            script,
            base_url: self.base_url(),
            target: DomEventTarget::node(self, node).data_handle(),
            retention: NodeRetention::new(self, node),
        };
        Ok(self.lease_script_activation(ctx, pending))
    }

    /// Invalid external sources fail during preparation, before any resource
    /// activation. The actual target lease survives detachment and GC until
    /// the normal asynchronous HTML error task runs.
    fn queue_script_preparation_error(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<()> {
        let lease = self.retain_script_target(ctx, node)?;
        self.scripts.borrow_mut().mark_started(node);
        let context = self.browsing_context()
            .filter(|context| browsing_context::is_active_document(context, self));
        let Some(context) = context else { return Ok(()); };
        let owner = browsing_context::context_realm_handle(&context);
        ctx.with_host_realm(&owner, |ctx| scheduling::queue_task(ctx, move |ctx| {
            lease.dispatch_terminal(ctx, "error").map(|_| ())
        })).map_err(browsing_context::host_realm_error)?
    }

    fn flush_script_activations(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if !self.has_browsing_context {
            return Ok(());
        }
        self.flush_pending_iframe_post_connections(ctx);
        loop {
            let Some(node) = self.scripts.borrow_mut().take_next() else {
                break;
            };
            let kind = {
                let session = self.session.borrow();
                let document = session.document();
                if document.kind(node).is_err()
                    || !script_loading::is_script(document, node)
                    || !script_loading::is_connected(document, node)
                    || self.scripts.borrow().already_started(node)
                    || self.scripts.borrow().parser_inserted(node)
                {
                    continue;
                }
                script_loading::prepare_kind(document, node, self.is_html_document)
            };
            let Some(kind) = kind else {
                continue;
            };
            match kind {
                script_loading::ScriptKind::DataBlock => {}
                script_loading::ScriptKind::InvalidSource => {
                    self.queue_script_preparation_error(ctx, node)?;
                }
                script_loading::ScriptKind::SuppressedClassic => {
                    self.scripts.borrow_mut().mark_started(node);
                }
                script_loading::ScriptKind::Unhandled(reason, src) => {
                    if (reason == UnhandledScriptReason::Module
                        && self.module_activations_enabled.get())
                        || (reason == UnhandledScriptReason::ExternalSource
                            && self.classic_resource_activations_enabled.get())
                    {
                        let script = {
                            let session = self.session.borrow();
                            let scripts = self.scripts.borrow();
                            script_loading::descriptor(
                                session.document(),
                                node,
                                self.is_html_document,
                                scripts.force_async(node),
                                scripts.parser_inserted(node),
                            )
                        };
                        if let Some(script) = script {
                            self.module_activations.borrow_mut().push_back(
                                PendingScriptActivation {
                                    script,
                                    base_url: self.base_url(),
                                    target: DomEventTarget::node(self, node).data_handle(),
                                    retention: NodeRetention::new(self, node),
                                },
                            );
                        }
                        self.scripts.borrow_mut().mark_started(node);
                        continue;
                    }
                    self.scripts.borrow_mut().mark_started(node);
                    self.scripts
                        .borrow_mut()
                        .record_unhandled(node, reason, src);
                }
                script_loading::ScriptKind::ClassicInline(source) => {
                    // Set the flag before evaluating: the script may re-enter DOM
                    // insertion APIs or remove and reinsert its own element.
                    self.scripts.borrow_mut().mark_started(node);
                    let completion = {
                        let _current_script = self.enter_script(Some(node));
                        let global = ctx.global_object();
                        ctx.eval_in_realm(&global, &source)
                    };
                    if let Err(exception) = completion {
                        Self::report_exception(ctx, lumen::embed::abrupt_value(exception));
                    }
                }
            }
        }
        Ok(())
    }

    fn queue_iframe_post_connections(
        &self,
        document: &lumen_html::Document,
        roots: impl Iterator<Item = NodeId>,
    ) {
        if !self.has_browsing_context {
            return;
        }
        let mut pending = self.pending_iframe_post_connections.borrow_mut();
        for root in roots {
            let mut current = Some(root);
            while let Some(node) = current {
                if is_html_iframe(document, node) && script_loading::is_connected(document, node) {
                    pending.push_back(node);
                }
                current =
                    lumen_html::selector::next_shadow_including_descendant(document, root, node)
                        .ok()
                        .flatten();
            }
        }
    }

    fn flush_pending_iframe_post_connections(self: &Rc<Self>, ctx: &mut Ctx) {
        loop {
            let Some(node) = self
                .pending_iframe_post_connections
                .borrow_mut()
                .pop_front()
            else {
                break;
            };
            let connected_iframe = {
                let session = self.session.borrow();
                let document = session.document();
                is_html_iframe(document, node) && script_loading::is_connected(document, node)
            };
            if !connected_iframe {
                continue;
            }
            let Ok(frame) = self.ensure_frame_context(ctx, node) else {
                continue;
            };
            if let Some(document) = frame.current_document() {
                document.process_initial_iframe_post_connections(ctx);
            }
            if frame.claim_initial_blank_load() {
                // No native session or frame borrow crosses event dispatch: a
                // load listener may detach, adopt, or reinsert this iframe.
                let still_connected = {
                    let session = self.session.borrow();
                    script_loading::is_connected(session.document(), node)
                };
                if still_connected {
                    let _ = self.dispatch_user_agent(ctx, node, "load", false, false, &[]);
                }
            }
        }
    }

    /// Run parser-inserted iframe post-connection steps after the embedder has
    /// installed the document URL and realm services, and before it runs author
    /// scripts. Dynamic insertions use the same steps from the mutation queue.
    pub fn process_initial_iframe_post_connections(self: &Rc<Self>, ctx: &mut Ctx) {
        if !self.has_browsing_context {
            return;
        }
        for node in self.connected_iframe_nodes() {
            self.pending_iframe_post_connections
                .borrow_mut()
                .push_back(node);
        }
        self.flush_pending_iframe_post_connections(ctx);
    }

    /// Report a script or listener exception against the active HTML global.
    /// Error reporting is guarded against recursive error-event dispatch and
    /// never propagates a new exception to the caller.
    pub fn report_exception(ctx: &mut Ctx, exception: Value) {
        error_reporting::report_exception(ctx, exception);
    }

    /// Mark a classic script as the document's current script while its source is evaluated.
    /// The returned guard restores the previous value on every exit path, including throws and
    /// nested script execution. Pass `None` while evaluating a module script.
    pub fn enter_script(self: &Rc<Self>, current: Option<NodeId>) -> CurrentScriptGuard {
        let previous = self.current_script.replace(current);
        let retention = current.map(|node| {
            let (owner, node) = self.resolve_adopted_node(node);
            *owner.retained_nodes.borrow_mut().entry(node).or_default() += 1;
            let retention = Rc::new(RefCell::new(CurrentScriptRetention {
                owner: owner.clone(),
                node,
            }));
            owner
                .script_retentions
                .borrow_mut()
                .entry(node)
                .or_default()
                .push(Rc::downgrade(&retention));
            retention
        });
        CurrentScriptGuard {
            realm: self.clone(),
            previous,
            retention,
        }
    }

    /// Store the embedder's URL for this live document. The HTML adapter keeps
    /// URL parsing in the browser/network owner while form submissions retain
    /// the correct base document context.
    pub fn set_document_url(&self, url: impl Into<String>) {
        let url = url.into();
        let previous_url = self.document_url();
        if previous_url.as_deref() == Some(url.as_str()) {
            return;
        }
        let previous_base = self.effective_base_url_from_cache();
        let first_assignment = previous_url.is_none();
        *self.document_identity.url.borrow_mut() = Some(url.clone());

        // During install the host supplies the response URL just after the
        // native document is created. If a getter ran before then, discard
        // that provisional about:blank calculation and freeze the initial
        // parsed base against the actual response fallback.
        if first_assignment {
            *self.document_base_url.borrow_mut() = DocumentBaseUrlCache::default();
        }
        if let Some(context) = self
            .browsing_context()
            .filter(|context| browsing_context::is_active_document(context, self))
        {
            browsing_context::update_document_origin(&context, &url);
            self.set_document_origin(browsing_context::context_origin(&context));
        }
        self.invalidate_frame_requests_if_base_changed(previous_base, first_assignment);
    }

    /// Compare the effective base only when a previous one was observed. Computing it here
    /// for an uninitialized cache would freeze a `<base href>` against a provisional fallback
    /// before the embedder has finished supplying the document URL and about base URL.
    fn invalidate_frame_requests_if_base_changed(
        &self,
        previous_base: Option<String>,
        force: bool,
    ) {
        let changed = match previous_base {
            Some(previous) if !force => previous != self.base_url(),
            _ => true,
        };
        if changed {
            self.invalidate_frame_navigation_requests();
        }
    }

    /// Set the inherited about base URL for an initial about:blank or srcdoc
    /// document. Embedders should call this while preparing a child document,
    /// before exposing its DOM to script.
    pub fn set_about_base_url(&self, url: Option<String>) {
        let url = url.and_then(|value| {
            lumen_common::url::parse(&value, None)
                .ok()
                .map(|parsed| parsed.href())
        });
        if *self.about_base_url.borrow() == url {
            return;
        }
        let previous_base = self.effective_base_url_from_cache();
        *self.about_base_url.borrow_mut() = url;
        self.invalidate_frame_requests_if_base_changed(previous_base, false);
    }

    pub fn document_url(&self) -> Option<String> {
        self.document_identity.url.borrow().clone()
    }

    pub fn document_encoding(&self) -> &'static str {
        self.document_encoding.get()
    }

    /// The resource host supplies the encoding used to decode this document.
    /// Invalid labels leave the existing value unchanged.
    pub fn set_document_encoding(&self, label: &str) -> bool {
        let Ok(encoding) = lumen_common::encoding::canonical_document_label(label) else {
            return false;
        };
        self.document_encoding.set(encoding);
        true
    }

    /// The fallback base URL, before any HTML `<base href>` element applies.
    fn fallback_base_url(&self) -> String {
        let document_url = self.document_url().unwrap_or_else(|| "about:blank".into());
        let Some(parsed) = lumen_common::url::parse(&document_url, None).ok() else {
            return document_url;
        };
        let about_url = parsed.scheme == "about"
            && parsed.host.is_none()
            && parsed.username.is_empty()
            && parsed.password.is_empty();
        if about_url && matches!(parsed.path.as_str(), "blank" | "srcdoc") {
            if let Some(inherited) = self.about_base_url.borrow().as_ref() {
                return inherited.clone();
            }
        }
        document_url
    }

    pub fn base_url(&self) -> String {
        let initialized = self.document_base_url.borrow().initialized;
        if !initialized {
            let session = self.session.borrow();
            self.recompute_document_base_url(session.document(), false);
        }
        let (has_base, frozen) = {
            let cache = self.document_base_url.borrow();
            (cache.first_base.is_some(), cache.frozen_url.clone())
        };
        if has_base {
            frozen.unwrap_or_else(|| self.fallback_base_url())
        } else {
            self.fallback_base_url()
        }
    }

    fn effective_base_url_from_cache(&self) -> Option<String> {
        let fallback = self.fallback_base_url();
        let cache = self.document_base_url.borrow();
        if !cache.initialized {
            return None;
        }
        Some(if cache.first_base.is_some() {
            cache.frozen_url.clone().unwrap_or(fallback)
        } else {
            fallback
        })
    }

    fn recompute_document_base_url(
        &self,
        document: &lumen_html::Document,
        refreeze_selected_base: bool,
    ) {
        let fallback = self.fallback_base_url();
        let first =
            first_html_base_with_href(document, document.root(), false, self.is_html_document);
        let (was_initialized, previous_base, previous_frozen) = {
            let cache = self.document_base_url.borrow();
            (
                cache.initialized,
                cache.first_base,
                cache.frozen_url.clone(),
            )
        };
        let (first_base, frozen_url) = match first {
            Some((node, href)) => {
                let frozen =
                    if was_initialized && previous_base == Some(node) && !refreeze_selected_base {
                        previous_frozen.unwrap_or_else(|| freeze_base_href(&href, &fallback))
                    } else {
                        freeze_base_href(&href, &fallback)
                    };
                (Some(node), Some(frozen))
            }
            None => (None, None),
        };
        *self.document_base_url.borrow_mut() = DocumentBaseUrlCache {
            initialized: true,
            first_base,
            frozen_url,
        };
    }

    fn observe_base_url_mutation(
        &self,
        document: &lumen_html::Document,
        mutation: &lumen_html::observe::ObservedMutation,
    ) -> bool {
        if !script_loading::is_connected(document, mutation.target) {
            return false;
        }
        let previous_base = self.effective_base_url_from_cache();
        let selected_base = self.document_base_url.borrow().first_base;
        let (relevant, refreeze_selected_base) = match &mutation.kind {
            lumen_html::observe::ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if attribute_name_is_href(name, self.is_html_document)
                && namespace_uri.is_none()
                && is_html_base(document, mutation.target) =>
            {
                let selected = selected_base == Some(mutation.target);
                let currently_eligible =
                    html_base_href(document, mutation.target, self.is_html_document).is_some();
                (selected || currently_eligible, selected)
            }
            lumen_html::observe::ObservedKind::ChildList { added, removed, .. } => {
                let relevant = mutation_subtree_has_base(
                    document,
                    added.iter().chain(removed.iter()).copied(),
                    self.is_html_document,
                );
                let refreeze = selected_base
                    .is_some_and(|selected| mutation_contains_node(document, mutation, selected));
                (relevant, refreeze)
            }
            lumen_html::observe::ObservedKind::ChildListMany { added, removed } => {
                let relevant = mutation_subtree_has_base(
                    document,
                    added.iter().chain(removed.iter()).copied(),
                    self.is_html_document,
                );
                let refreeze = selected_base
                    .is_some_and(|selected| mutation_contains_node(document, mutation, selected));
                (relevant, refreeze)
            }
            lumen_html::observe::ObservedKind::Attribute { .. }
            | lumen_html::observe::ObservedKind::CharacterData { .. }
            | lumen_html::observe::ObservedKind::SlotAssignment => (false, false),
        };
        if relevant {
            self.recompute_document_base_url(document, refreeze_selected_base);
            return previous_base
                .as_deref()
                .is_some_and(|previous| previous != self.base_url());
        }
        false
    }

    fn observe_frame_navigation_mutation(
        &self,
        document: &lumen_html::Document,
        mutation: &lumen_html::observe::ObservedMutation,
        base_changed: bool,
    ) {
        if base_changed {
            self.invalidate_frame_navigation_requests();
        }
        match &mutation.kind {
            lumen_html::observe::ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if namespace_uri.is_none()
                && is_html_iframe(document, mutation.target)
                && is_frame_navigation_attribute(name, self.is_html_document) =>
            {
                if let Some(context) = self
                    .frame_contexts
                    .borrow()
                    .get(&mutation.target)
                    .and_then(std::rc::Weak::upgrade)
                {
                    context.invalidate_owner_navigation_request();
                }
            }
            lumen_html::observe::ObservedKind::ChildList { added, removed, .. } => {
                self.invalidate_frame_contexts_in_subtrees(document, added.iter().copied());
                self.queue_iframe_post_connections(document, added.iter().copied());
                self.queue_removed_frame_contexts(document, removed.iter().copied());
            }
            lumen_html::observe::ObservedKind::ChildListMany { added, removed } => {
                self.invalidate_frame_contexts_in_subtrees(document, added.iter().copied());
                self.queue_iframe_post_connections(document, added.iter().copied());
                self.queue_removed_frame_contexts(document, removed.iter().copied());
            }
            lumen_html::observe::ObservedKind::Attribute { .. }
            | lumen_html::observe::ObservedKind::CharacterData { .. }
            | lumen_html::observe::ObservedKind::SlotAssignment => {}
        }
    }

    fn invalidate_frame_contexts_in_subtrees(
        &self,
        document: &lumen_html::Document,
        roots: impl Iterator<Item = NodeId>,
    ) {
        let contexts = self.frame_contexts.borrow();
        for root in roots {
            let mut current = Some(root);
            while let Some(node) = current {
                if let Some(context) = contexts.get(&node).and_then(std::rc::Weak::upgrade) {
                    context.invalidate_navigation_request();
                }
                current =
                    lumen_html::selector::next_shadow_including_descendant(document, root, node)
                        .ok()
                        .flatten();
            }
        }
    }

    fn queue_removed_frame_contexts(
        &self,
        document: &lumen_html::Document,
        roots: impl Iterator<Item = NodeId>,
    ) {
        let contexts = self.frame_contexts.borrow();
        let mut pending = self.pending_frame_contexts.borrow_mut();
        for root in roots {
            let mut current = Some(root);
            while let Some(node) = current {
                if let Some(context) = contexts.get(&node).and_then(std::rc::Weak::upgrade) {
                    context.invalidate_navigation_request();
                    pending.entry(node).or_insert(context);
                }
                current =
                    lumen_html::selector::next_shadow_including_descendant(document, root, node)
                        .ok()
                        .flatten();
            }
        }
    }

    fn invalidate_frame_navigation_requests(&self) {
        for weak in self.frame_contexts.borrow().values() {
            if let Some(context) = weak.upgrade() {
                context.invalidate_navigation_request();
            }
        }
    }

    /// Advance the parsed document through the browser's ready-state lifecycle.
    /// Each forward transition dispatches one non-bubbling `readystatechange`
    /// event at the Document target.
    pub fn set_document_ready_state(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        state: DocumentReadyState,
    ) -> OpResult<()> {
        let current = self.ready_state.get();
        if state <= current {
            return Ok(());
        }
        self.ready_state.set(state);
        let root = self.session.borrow().document().root();
        self.dispatch(ctx, root, "readystatechange", false, false, &[])
            .map(|_| ())
    }

    fn open_document_stream(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if !self.is_html_document {
            return Err(OpError::new(
                "InvalidStateError",
                "document.open() requires an HTML document",
            ));
        }
        if self.current_script.get().is_some()
            && self
                .document_parser
                .borrow()
                .as_ref()
                .is_some_and(html::HtmlDocumentParser::is_paused_for_script)
        {
            // The HTML document.open algorithm is a no-op while a parser-
            // inserted script is still on the parser's script nesting stack.
            return Ok(());
        }
        self.erase_document_open_listeners(ctx)?;
        // A permitted new stream supersedes any prior parser and its buffered
        // source. The parser-script nesting guard above preserves the active
        // stream when document.open() is called from a parser-inserted script.
        self.parser_generation
            .set(self.parser_generation.get().wrapping_add(1));
        self.document_parser.borrow_mut().take();

        let options = {
            let session = self.session.borrow();
            let document = session.document();
            html::ParseOptions {
                allow_declarative_shadow_roots: document.allow_declarative_shadow_roots(),
                scripting_enabled: document.scripting_enabled(),
            }
        };
        let (parser, removed) = {
            let mut session = self.session.borrow_mut();
            let opened = html::HtmlDocumentParser::open(session.document_mut(), options)
                .map_err(html_parser_error)?;
            // The document.open algorithm starts in no-quirks mode. A doctype
            // parsed from this stream can still select limited quirks or quirks.
            session
                .document_mut()
                .set_document_mode(lumen_html::DocumentMode::NoQuirks);
            opened
        };
        self.reap_detached(removed);
        *self.document_parser.borrow_mut() = Some(parser);
        *self.scripts.borrow_mut() = script_loading::ScriptLoader::default();
        self.module_activations.borrow_mut().clear();
        self.ready_state.set(DocumentReadyState::Loading);
        let root = self.session.borrow().document().root();
        self.dispatch(ctx, root, "readystatechange", false, false, &[])
            .map(|_| ())
    }

    fn erase_document_open_listeners(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        {
            let session = self.session.borrow();
            let document = session.document();
            let root = document.root();
            let mut current = Some(root);
            while let Some(node) = current {
                let owner = if node == root {
                    self.document_wrapper
                        .borrow()
                        .as_ref()
                        .and_then(WeakValue::upgrade)
                } else {
                    self.wrappers
                        .borrow()
                        .get(&node)
                        .and_then(WeakValue::upgrade)
                };
                events::erase_node_listeners(ctx, self, node, owner.as_ref());
                current =
                    lumen_html::selector::next_shadow_including_descendant(document, root, node)
                        .map_err(dom_error)?;
            }
        }
        let owner = self
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade);
        events::erase_window_listeners(ctx, self, owner.as_ref());
        Ok(())
    }

    fn write_document_stream(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        chunks: &[String],
        append_newline: bool,
    ) -> OpResult<()> {
        if !self.is_html_document {
            return Err(OpError::new(
                "InvalidStateError",
                "document.write() requires an HTML document",
            ));
        }
        if self.document_parser.borrow().is_none() {
            self.open_document_stream(ctx)?;
        }
        let active_script = self.current_script.get();
        let paused_for_script = self
            .document_parser
            .borrow()
            .as_ref()
            .is_some_and(html::HtmlDocumentParser::is_paused_for_script);
        if let (Some(parent_script), true) = (active_script, paused_for_script) {
            let script = {
                let mut parser = self.document_parser.borrow_mut();
                let parser = parser.as_mut().ok_or_else(|| {
                    OpError::new("InvalidStateError", "document parser is unavailable")
                })?;
                let mut session = self.session.borrow_mut();
                parser
                    .write_at_script_position_until_script(
                        session.document_mut(),
                        parent_script,
                        chunks,
                        append_newline,
                    )
                    .map_err(html_parser_error)?
            };
            return self.run_inserted_parser_scripts(ctx, parent_script, script);
        }

        let script = {
            let mut parser = self.document_parser.borrow_mut();
            let parser = parser.as_mut().ok_or_else(|| {
                OpError::new("InvalidStateError", "document parser is unavailable")
            })?;
            let mut session = self.session.borrow_mut();
            parser
                .write_parts_until_script(session.document_mut(), chunks, append_newline)
                .map_err(html_parser_error)?
        };
        self.run_document_parser_scripts(ctx, script)
    }

    fn close_document_stream(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if self.document_parser.borrow().is_none() {
            return Ok(());
        }
        let (script, paused) = {
            let mut parser = self.document_parser.borrow_mut();
            let parser = parser.as_mut().ok_or_else(|| {
                OpError::new("InvalidStateError", "document parser is unavailable")
            })?;
            let mut session = self.session.borrow_mut();
            let script = parser
                .finish_until_script(session.document_mut())
                .map_err(html_parser_error)?;
            (script, parser.is_paused_for_script())
        };
        if paused && script.is_none() {
            // document.close() called by the currently executing parser script
            // records EOF. The outer parser pump resumes only after evaluation
            // returns, so it cannot recurse into its own tree builder here.
            return Ok(());
        }
        self.run_document_parser_scripts(ctx, script)
    }

    fn run_document_parser_scripts(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        mut script: Option<NodeId>,
    ) -> OpResult<()> {
        let generation = self.parser_generation.get();
        loop {
            if self.parser_generation.get() != generation {
                return Ok(());
            }
            let Some(node) = script else {
                return self.finish_document_stream_if_closed(ctx, generation);
            };
            self.execute_document_parser_script(ctx, node)?;

            if self.parser_generation.get() != generation {
                return Ok(());
            }
            script = {
                let mut parser = self.document_parser.borrow_mut();
                let parser = parser.as_mut().ok_or_else(|| {
                    OpError::new("InvalidStateError", "document parser is unavailable")
                })?;
                let mut session = self.session.borrow_mut();
                parser
                    .resume_until_script(session.document_mut())
                    .map_err(html_parser_error)?
            };
        }
    }

    fn run_inserted_parser_scripts(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        parent_script: NodeId,
        mut script: Option<NodeId>,
    ) -> OpResult<()> {
        let generation = self.parser_generation.get();
        while let Some(node) = script {
            if self.parser_generation.get() != generation {
                return Ok(());
            }
            self.execute_document_parser_script(ctx, node)?;
            if self.parser_generation.get() != generation {
                return Ok(());
            }
            script = {
                let mut parser = self.document_parser.borrow_mut();
                let parser = parser.as_mut().ok_or_else(|| {
                    OpError::new("InvalidStateError", "document parser is unavailable")
                })?;
                let mut session = self.session.borrow_mut();
                parser
                    .resume_insertion_until_script(session.document_mut(), node, parent_script)
                    .map_err(html_parser_error)?
            };
        }
        Ok(())
    }

    fn execute_document_parser_script(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
    ) -> OpResult<()> {
        self.scripts.borrow_mut().register_parser_script(node);
        let (scripting_enabled, kind) = {
            let session = self.session.borrow();
            let document = session.document();
            (
                document.scripting_enabled(),
                script_loading::prepare_kind(document, node, self.is_html_document),
            )
        };
        let active_context = self.browsing_context().filter(|context| {
            self.has_browsing_context && browsing_context::is_active_document(context, self)
        });
        if !scripting_enabled || active_context.is_none() {
            self.scripts.borrow_mut().mark_started(node);
            return Ok(());
        }
        match kind {
            Some(script_loading::ScriptKind::InvalidSource) => {
                self.queue_script_preparation_error(ctx, node)?;
            }
            Some(script_loading::ScriptKind::ClassicInline(source)) => {
                self.scripts.borrow_mut().mark_started(node);
                let active_context = active_context.expect("active context checked above");
                let handle = browsing_context::context_realm_handle(&active_context);
                ctx.with_host_realm(&handle, |ctx| {
                    let _current_script = self.enter_script(Some(node));
                    let global = ctx.global_object();
                    if let Err(exception) = ctx.eval_in_realm(&global, &source) {
                        Self::report_exception(ctx, lumen::embed::abrupt_value(exception));
                    }
                })
                .map_err(browsing_context::host_realm_error)?;
            }
            Some(script_loading::ScriptKind::SuppressedClassic) => {
                self.scripts.borrow_mut().mark_started(node);
            }
            Some(script_loading::ScriptKind::Unhandled(reason, src)) => {
                // External and module scripts need the embedder's resource
                // loader and parser-blocking queue. Keep that limitation
                // explicit rather than running the buffered tail early and
                // pretending those scripts completed synchronously.
                self.scripts.borrow_mut().mark_started(node);
                self.scripts
                    .borrow_mut()
                    .record_unhandled(node, reason, src);
            }
            Some(script_loading::ScriptKind::DataBlock) | None => {
                self.scripts.borrow_mut().mark_parser_prepared(node);
            }
        }
        Ok(())
    }

    fn finish_document_stream_if_closed(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        generation: u64,
    ) -> OpResult<()> {
        if self.parser_generation.get() != generation {
            return Ok(());
        }
        let closed = self
            .document_parser
            .borrow()
            .as_ref()
            .is_some_and(html::HtmlDocumentParser::is_closed);
        if !closed {
            return Ok(());
        }
        self.document_parser.borrow_mut().take();
        self.set_document_ready_state(ctx, DocumentReadyState::Interactive)?;
        if self.parser_generation.get() != generation {
            return Ok(());
        }
        self.set_document_ready_state(ctx, DocumentReadyState::Complete)
    }

    pub fn set_file_picker_host(
        &self,
        callback: Rc<dyn Fn(forms::FilePickerRequest) -> Result<(), String>>,
    ) {
        *self.file_picker_host.borrow_mut() = Some(callback);
    }

    pub fn request_file_picker(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<()> {
        let request = forms::file_picker_request(self, node)?;
        let callback = self.file_picker_host.borrow().clone().ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "the embedder has not supplied a file picker",
            )
        })?;
        if !self.consume_user_activation() {
            return Err(OpError::new(
                "NotAllowedError",
                "showPicker requires trusted user activation",
            ));
        }
        self.notify_clipboard_permissions_changed(ctx)?;
        callback(request).map_err(|message| OpError::new("NotAllowedError", message))
    }

    pub fn set_form_submission_host(
        &self,
        callback: Rc<dyn Fn(forms::FormSubmissionRequest) -> Result<(), String>>,
    ) {
        self.set_form_submission_host_with_context(Rc::new(move |_ctx, request| callback(request)));
    }

    /// Install a navigation host that can resolve a submission's target in the
    /// current realm after its formdata listeners have completed.
    pub fn set_form_submission_host_with_context(
        &self,
        callback: Rc<dyn Fn(&mut Ctx, forms::FormSubmissionRequest) -> Result<(), String>>,
    ) {
        *self.form_submission_host.borrow_mut() = Some(callback);
    }

    pub fn submit_form(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        form: NodeId,
        submitter: Option<NodeId>,
    ) -> OpResult<bool> {
        self.submit_form_with_mode(ctx, form, submitter, false)
    }

    pub fn submit_form_legacy(self: &Rc<Self>, ctx: &mut Ctx, form: NodeId) -> OpResult<bool> {
        self.submit_form_with_mode(ctx, form, None, true)
    }

    fn submit_form_with_mode(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        form: NodeId,
        submitter: Option<NodeId>,
        legacy: bool,
    ) -> OpResult<bool> {
        let request = if legacy {
            forms::legacy_submission(ctx, self, form, &self.forms)?
        } else {
            forms::request_submission(ctx, self, form, submitter, &self.forms)?
        };
        let Some(request) = request else {
            return Ok(false);
        };
        let callback = self.form_submission_host.borrow().clone().ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "the embedder has not supplied form navigation",
            )
        })?;
        callback(ctx, request).map_err(|message| OpError::new("NotSupportedError", message))?;
        Ok(true)
    }

    pub(crate) fn resolve_adopted_node(self: &Rc<Self>, mut id: NodeId) -> (Rc<DomRealm>, NodeId) {
        let mut realm = self.clone();
        // Repeated adoption can chain old IDs through several documents. Keep
        // following those links so retained static collections still resolve
        // to the node's current owning document.
        for _ in 0..64 {
            let next = realm.adopted_nodes.borrow().get(&id).cloned();
            let Some((next_realm, next_id)) = next else {
                break;
            };
            let Some(next_realm) = next_realm.upgrade() else {
                break;
            };
            if Rc::ptr_eq(&realm, &next_realm) && id == next_id {
                break;
            }
            realm = next_realm;
            id = next_id;
        }
        (realm, id)
    }

    pub fn set_layout_flusher(
        &self,
        callback: Rc<dyn Fn(&mut RenderSession) -> Result<(), String>>,
    ) {
        *self.layout_flusher.borrow_mut() = Some(callback);
    }

    /// Supply the embedder's bounded resource resolver for HTML image
    /// elements. The resolver owns fetching and decoding; this realm tracks
    /// selection, completion, dimensions, and queued DOM events.
    pub fn set_image_resolver(&self, resolver: Rc<dyn lumen_html::layout::ImageResolver>) {
        self.images.set_resolver(resolver);
    }

    /// Audio nodes with explicit `src` resources, including detached `Audio()` elements.
    pub fn media_sources(&self) -> Vec<(NodeId, String)> {
        let document = self.session.borrow();
        let mut found = Vec::new();
        let mut stack = vec![document.document().root()];
        let mut visited = std::collections::HashSet::new();
        for node in self.media.borrow().detached_nodes() {
            visited.insert(node);
            if let Ok(NodeKind::Element { attributes, .. }) = document.document().kind(node) {
                if let Some((_, source)) = attributes
                    .iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case("src"))
                {
                    if !source.is_empty() {
                        found.push((node, source.clone()));
                    }
                }
            }
        }
        while let Some(node) = stack.pop() {
            if !visited.insert(node) {
                continue;
            }
            if let Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                attributes,
            }) = document.document().kind(node)
            {
                if name.eq_ignore_ascii_case("audio") {
                    if let Some((_, source)) = attributes
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("src"))
                    {
                        if !source.is_empty() {
                            found.push((node, source.clone()));
                        }
                    }
                }
            }
            let mut child = document.document().first_child(node).ok().flatten();
            while let Some(id) = child {
                stack.push(id);
                child = document.document().next_sibling(id).ok().flatten();
            }
        }
        found
    }

    /// Explicit video `src` resources, including detached `Video()` elements.
    pub fn video_sources(&self) -> Vec<(NodeId, String)> {
        let document = self.session.borrow();
        let mut found = Vec::new();
        let mut stack = vec![document.document().root()];
        let mut visited = std::collections::HashSet::new();
        for node in self.media.borrow().detached_nodes() {
            visited.insert(node);
            if matches!(document.document().kind(node), Ok(NodeKind::Element { name, .. }) if name.eq_ignore_ascii_case("video"))
            {
                if let Ok(NodeKind::Element { attributes, .. }) = document.document().kind(node) {
                    if let Some((_, source)) = attributes
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("src"))
                    {
                        if !source.is_empty() {
                            found.push((node, source.clone()));
                        }
                    }
                }
            }
        }
        while let Some(node) = stack.pop() {
            if !visited.insert(node) {
                continue;
            }
            if let Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                attributes,
            }) = document.document().kind(node)
            {
                if name.eq_ignore_ascii_case("video") {
                    if let Some((_, source)) = attributes
                        .iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case("src"))
                    {
                        if !source.is_empty() {
                            found.push((node, source.clone()));
                        }
                    }
                }
            }
            let mut child = document.document().first_child(node).ok().flatten();
            while let Some(id) = child {
                stack.push(id);
                child = document.document().next_sibling(id).ok().flatten();
            }
        }
        found
    }

    pub fn media_snapshot(&self, node: NodeId) -> MediaSnapshot {
        let default_muted = self.media_default_muted(node);
        let media = self.media.borrow();
        let mut snapshot = media.snapshot(node);
        snapshot.muted = media.muted(node, default_muted);
        snapshot
    }

    fn media_default_muted(&self, node: NodeId) -> bool {
        self.session.borrow().document().get_attribute_ns_ref(node, None, "muted")
            .ok().flatten().is_some()
    }

    pub(crate) fn media_set_muted(&self, node: NodeId, value: bool) {
        let default_muted = self.media_default_muted(node);
        let mut media = self.media.borrow_mut();
        let previous = media.muted(node, default_muted);
        media.set_muted(node, value, previous);
    }

    pub fn media_select_source(&self, node: NodeId, source: String) -> u64 {
        let old_generation = self.media.borrow().snapshot(node).generation;
        let generation = self.media.borrow_mut().set_source(node, source);
        if generation != old_generation
            && matches!(self.session.borrow().document().kind(node), Ok(NodeKind::Element { name, namespace: Namespace::Html, .. }) if name == "video")
        {
            let _ = self.session.borrow_mut().set_node_bitmap(node, None);
        }
        generation
    }

    pub fn media_reload(&self, ctx: &mut Ctx, node: NodeId) -> u64 {
        let generation = self.media.borrow_mut().reload(ctx, node);
        if matches!(self.session.borrow().document().kind(node), Ok(NodeKind::Element { name, namespace: Namespace::Html, .. }) if name == "video")
        {
            let _ = self.session.borrow_mut().set_node_bitmap(node, None);
        }
        generation
    }

    pub fn media_loaded(&self, node: NodeId, generation: u64, duration: f64) {
        self.media.borrow_mut().loaded(node, generation, duration);
    }

    pub fn media_video_loaded(
        &self,
        node: NodeId,
        generation: u64,
        duration: f64,
        width: u32,
        height: u32,
        origin_clean: bool,
    ) {
        self.media.borrow_mut().video_loaded(
            node,
            generation,
            duration,
            width,
            height,
            origin_clean,
        );
    }

    pub fn media_set_video_frame(
        &self,
        node: NodeId,
        generation: u64,
        frame: Option<VideoFrameSnapshot>,
    ) -> Result<(), String> {
        self.media
            .borrow_mut()
            .set_video_frame(node, generation, frame);
        let frame = self.media.borrow().video_frame(node);
        let image = frame.map(|frame| {
            std::sync::Arc::new(lumen_html::paint::ImageData {
                width: frame.width,
                height: frame.height,
                pixels: frame.rgba.as_ref().clone(),
            })
        });
        self.session
            .borrow_mut()
            .set_node_bitmap(node, image)
            .map_err(|error| format!("video frame presentation failed: {error:?}"))
    }

    pub fn media_video_frame(&self, node: NodeId) -> Option<VideoFrameSnapshot> {
        self.media.borrow().video_frame(node)
    }

    pub fn media_failed(&self, node: NodeId, generation: u64, message: String) {
        self.media.borrow_mut().failed(node, generation, message);
    }

    pub fn media_play_started(&self, ctx: &mut Ctx, node: NodeId, generation: u64) {
        self.media.borrow_mut().playing(ctx, node, generation);
    }

    pub fn media_progress(&self, node: NodeId, generation: u64, current_time: f64) {
        self.media
            .borrow_mut()
            .progress(node, generation, current_time);
    }

    pub fn media_ended(&self, node: NodeId, generation: u64, duration: f64) {
        self.media.borrow_mut().ended(node, generation, duration);
    }

    pub fn media_reject_play(&self, ctx: &mut Ctx, node: NodeId, message: &str) {
        self.media.borrow_mut().reject_play(ctx, node, message);
    }

    pub fn media_pause_node(&self, ctx: &mut Ctx, node: NodeId) {
        self.media.borrow_mut().pause(ctx, node);
    }

    pub fn queue_media_tasks(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<usize> {
        let events = self.media.borrow_mut().take_events();
        let count = events.len();
        for (node, generation, kind) in events {
            if self.session.borrow().document().kind(node).is_err() {
                continue;
            }
            let wrapper = self.wrap(ctx, node);
            let retention = NodeRetention::new(self, node);
            let realm = self.clone();
            scheduling::queue_task(ctx, move |ctx| {
                let (_wrapper, _retention) = (wrapper, retention);
                if (kind == "ratechange" || realm.media_snapshot(node).generation == generation)
                    && realm.session.borrow().document().kind(node).is_ok()
                {
                    realm.dispatch_user_agent(ctx, node, &kind, false, false, &[])?;
                }
                Ok(())
            })?;
        }
        Ok(count)
    }

    /// Invalidate retained HTML layout and paint after host-managed image data changes.
    pub fn invalidate_image_resources(&self) {
        self.session.borrow_mut().invalidate_image_resources();
    }

    /// Nodes with active image requests, including detached `new Image()` nodes.
    /// The embedder uses these to start managed resource fetches.
    pub fn pending_image_nodes(&self) -> Vec<NodeId> {
        self.images.pending_nodes()
    }

    /// Supply the embedder's font fetcher and decoder. The same decoded face
    /// can then be used by both FontFaceSet and the render provider.
    pub fn set_font_resource_loader(&self, loader: Rc<dyn FontResourceLoader>) {
        self.font_loading.set_provider(loader);
    }

    /// Fetch and decode CSS-connected font faces queued by FontFaceSet.load.
    /// The embedder calls this from its existing user-agent task pump.
    pub fn queue_font_tasks(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<usize> {
        let base = self.base_url();
        let count = self.font_loading.pump(ctx, &base);
        if count != 0 {
            self.session.borrow_mut().invalidate_fonts();
        }
        Ok(count)
    }

    /// Complete font-set lifecycle only after the embedder has updated layout
    /// with the current decoded faces. `false` means pending work or queued events.
    pub fn settle_font_loading(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<bool> {
        if self.font_loading.has_live_sets() {
            self.flush_layout()?;
        }
        self.font_loading.settle(ctx)
    }

    /// Resolve image requests whose sources have changed and enqueue their
    /// `load`/`error` events as user-agent tasks. Events are dispatched only
    /// when the host runs the existing scheduling task queue.
    pub fn queue_image_tasks(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<usize> {
        let base = self.base_url();
        let completions = {
            let session = self.session.borrow();
            self.images.queue_completions(session.document(), &base)
        };
        let mut event_roots = Vec::with_capacity(completions.len());
        {
            let mut roots = self.image_request_roots.borrow_mut();
            if !roots.is_empty() {
                roots.retain(|&node, _| {
                    self.images.is_loading(node)
                        || completions.iter().any(|completion| completion.node == node)
                });
            }
            let missing = self.images.take_unknown_pending(&roots);
            for &node in &missing {
                roots.insert(node, self.retain_image_request(ctx, node));
            }
            self.images.return_pending_scratch(missing);
            for completion in &completions {
                let retained = roots
                    .remove(&completion.node)
                    .unwrap_or_else(|| self.retain_image_request(ctx, completion.node));
                event_roots.push(self.retain_image_event(ctx, completion.node, retained));
            }
        }
        self.sync_image_bitmaps()?;

        let count = completions.len();
        for (completion, root) in completions.into_iter().zip(event_roots) {
            let realm = self.clone();
            scheduling::queue_task(ctx, move |ctx| {
                // The active image algorithm keeps both the detached node's
                // wrapper and its event target alive through event dispatch.
                let _root = root;
                let node_exists = realm
                    .session
                    .borrow()
                    .document()
                    .kind(completion.node)
                    .is_ok();
                if node_exists
                    && realm
                        .images
                        .completion_is_current(completion.node, completion.generation)
                {
                    realm.dispatch(
                        ctx,
                        completion.node,
                        completion.kind.as_str(),
                        false,
                        false,
                        &[],
                    )?;
                }
                Ok(())
            })?;
        }
        Ok(count)
    }

    /// A source change starts an image request that must keep its wrapper's
    /// listeners alive before the next pump, even for a detached `new Image()`.
    fn retain_dirty_image_request(self: &Rc<Self>, node: NodeId) {
        if !self.images.has_dirty_source(node) {
            return;
        }
        let Some(target) = self
            .targets
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
        else {
            return;
        };
        let Ok(mut roots) = self.image_request_roots.try_borrow_mut() else {
            return;
        };
        roots.entry(node).or_insert_with(|| ImageRequestRoot {
            target,
            retention: NodeRetention::new(self, node),
        });
    }

    fn retain_image_request(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> ImageRequestRoot {
        let wrapper = self.wrap(ctx, node);
        let target = self
            .targets
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
            .expect("wrapped image node has an event target");
        drop(wrapper);
        ImageRequestRoot {
            target,
            retention: NodeRetention::new(self, node),
        }
    }

    fn retain_image_event(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        retained: ImageRequestRoot,
    ) -> ImageEventRoot {
        ImageEventRoot {
            _wrapper: self.wrap(ctx, node),
            _target: retained.target,
            _retention: retained.retention,
        }
    }

    fn sync_image_bitmaps(&self) -> OpResult<()> {
        if self.images.has_dirty_requests() {
            let base = self.base_url();
            self.images
                .synchronize_dirty(self.session.borrow().document(), &base);
        }
        let updates = self.images.take_bitmap_updates();
        if updates.is_empty() {
            return Ok(());
        }
        let mut session = self.session.borrow_mut();
        for (node, image) in updates {
            if session.document().kind(node).is_err() {
                continue;
            }
            session.set_node_bitmap(node, image).map_err(|_| {
                OpError::new(
                    "InvalidStateError",
                    "could not publish the current image bitmap",
                )
            })?;
        }
        Ok(())
    }

    pub(crate) fn flush_layout(&self) -> OpResult<()> {
        self.sync_image_bitmaps()?;
        self.sync_canvas()?;
        let callback = self.layout_flusher.borrow().clone();
        if let Some(callback) = callback {
            callback(&mut self.session.borrow_mut())
                .map_err(|message| OpError::new("InvalidStateError", message))?;
        } else if self.session.borrow().viewport_size().is_none() {
            return Err(OpError::new(
                "InvalidStateError",
                "The embedder has not supplied a layout provider",
            ));
        }
        Ok(())
    }

    /// Publish bitmap changes queued by DOM mutations before borrowing the render session.
    pub fn sync_canvas(&self) -> OpResult<()> {
        self.canvases.sync(self)
    }
    pub(crate) fn add_mutation_sink(
        &self,
        sink: Rc<dyn Fn(&lumen_html::Document, &lumen_html::observe::ObservedMutation)>,
    ) {
        self.mutation_sinks.borrow_mut().push(sink);
    }
    pub(crate) fn value_epoch(&self) -> u64 {
        self.programmatic_value_epoch.get()
    }

    pub(crate) fn value_changed_since(&self, node: NodeId, epoch: u64) -> bool {
        let current = self.value_epoch();
        self.programmatic_value_writes
            .borrow()
            .changed_since(node, epoch, current)
    }

    pub(crate) fn invalidate_editing_for_value_change(&self, node: NodeId) {
        let sequence = self.programmatic_value_epoch.get().saturating_add(1);
        self.programmatic_value_epoch.set(sequence);
        self.programmatic_value_writes
            .borrow_mut()
            .record(sequence, node);
        self.editing.borrow_mut().invalidate(node);
    }

    pub(crate) fn invalidate_textarea_ancestor(&self, node: NodeId) {
        let textarea = {
            let session = self.session.borrow();
            let document = session.document();
            let mut current = Some(node);
            let mut found = None;
            while let Some(id) = current {
                if matches!(document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "textarea")
                {
                    found = Some(id);
                    break;
                }
                current = document.parent(id).ok().flatten();
            }
            found
        };
        if let Some(textarea) = textarea {
            self.invalidate_editing_for_value_change(textarea);
        }
    }

    pub fn focused_node(&self) -> Option<NodeId> {
        let node = self.focused.get()?;
        let session = self.session.borrow();
        let document = session.document();
        let mut ancestor = node;
        loop {
            if ancestor == document.root() {
                return Some(node);
            }
            ancestor = document.shadow_including_parent(ancestor).ok().flatten()?;
        }
    }

    fn interaction_element(&self, node: NodeId) -> Option<NodeId> {
        let session = self.session.borrow();
        let document = session.document();
        let mut current = Some(node);
        while let Some(candidate) = current {
            if matches!(document.kind(candidate), Ok(NodeKind::Element { .. })) {
                let mut ancestor = candidate;
                loop {
                    if ancestor == document.root() {
                        return Some(candidate);
                    }
                    ancestor = document.shadow_including_parent(ancestor).ok().flatten()?;
                }
            }
            current = document.composed_parent(candidate).ok().flatten();
        }
        None
    }

    fn publish_interaction_state(&self) {
        let focused = self.focused_node();
        let focus_visible = self
            .focus_visible
            .get()
            .filter(|node| Some(*node) == focused);
        let hover = self
            .hover_target
            .get()
            .and_then(|node| self.interaction_element(node));
        let active = self.active_targets.get().map(|node| {
            node.and_then(|node| self.interaction_element(node))
        });
        self.session
            .borrow_mut()
            .document_mut()
            .set_interaction_state(lumen_html::interaction::InteractionState {
                focused,
                focus_visible,
                hover,
                active,
            });
    }

    /// Publish the current pointing-device target before pointer transition
    /// events run, so synchronous selector queries observe the new state.
    pub(crate) fn publish_hover_target(&self, target: Option<NodeId>) {
        let target = target.and_then(|node| self.interaction_element(node));
        self.hover_target.set(target);
        self.publish_interaction_state();
    }

    /// Return the pointer target plus a label-associated control, if present.
    /// Each anchor is matched through the shared flat-tree ancestor algorithm.
    pub(crate) fn active_targets_for_pointer(&self, target: NodeId) -> [Option<NodeId>; 2] {
        let Some(target) = self.interaction_element(target) else {
            return [None, None];
        };
        let associated = {
            let session = self.session.borrow();
            let document = session.document();
            let mut current = Some(target);
            let mut associated = None;
            while let Some(node) = current {
                if lumen_html::forms::html_element_local_name(document, node) == Some("label") {
                    associated = lumen_html::labels::label_control(document, node)
                        .ok()
                        .flatten();
                    break;
                }
                current = document.composed_parent(node).ok().flatten();
            }
            associated.filter(|node| *node != target)
        };
        [Some(target), associated]
    }

    /// Publish active anchors before down events and clear them before up/click
    /// events. Pointer helpers own the event sequence and call this at its
    /// observable transition points.
    pub(crate) fn publish_active_targets(
        &self,
        primary: Option<NodeId>,
        associated: Option<NodeId>,
    ) {
        let targets = [
            primary.and_then(|node| self.interaction_element(node)),
            associated.and_then(|node| self.interaction_element(node)),
        ];
        self.active_targets.set(targets);
        self.publish_interaction_state();
    }

    pub(crate) fn note_pointer_modality(&self) {
        self.keyboard_modality.set(false);
    }

    pub(crate) fn note_keyboard_modality(&self) {
        self.keyboard_modality.set(true);
        self.focus_visible.set(self.focused_node());
        self.publish_interaction_state();
    }

    fn focus_always_visible(&self, node: NodeId) -> bool {
        let session = self.session.borrow();
        let document = session.document();
        if !matches!(document.kind(node), Ok(NodeKind::Element { namespace: lumen_html::Namespace::Html, .. })) {
            return false;
        }
        match lumen_html::forms::html_element_local_name(document, node) {
            Some("textarea") => true,
            Some("input") => {
                let input_type = document
                    .get_attribute_ns_ref(node, None, "type")
                    .ok()
                    .flatten()
                    .unwrap_or("text");
                ![
                    "hidden", "button", "reset", "submit", "image", "checkbox", "radio",
                    "file", "range", "color",
                ]
                .iter()
                .any(|kind| input_type.eq_ignore_ascii_case(kind))
            }
            _ => document
                .get_attribute_ns_ref(node, None, "contenteditable")
                .ok()
                .flatten()
                .is_some_and(|value| {
                    value.is_empty()
                        || value.eq_ignore_ascii_case("true")
                        || value.eq_ignore_ascii_case("plaintext-only")
                }),
        }
    }

    pub fn focus(self: &Rc<Self>, ctx: &mut Ctx, node: Option<NodeId>) -> OpResult<()> {
        self.focus_with_cause(ctx, node, false)
    }

    pub(crate) fn focus_from_pointer(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: Option<NodeId>,
    ) -> OpResult<()> {
        self.note_pointer_modality();
        self.focus_with_cause(ctx, node, true)
    }

    /// Whether an element can receive focus using the same rendered, disabled,
    /// and connectedness checks as the actual focus algorithm. Dialog focus
    /// fallback uses this rather than maintaining a second focusability list.
    pub(crate) fn focusable_node(&self, node: NodeId) -> OpResult<bool> {
        if !self.focus_rendered(node)? {
            return Ok(false);
        }
        let session = self.session.borrow();
        let document = session.document();
        let NodeKind::Element {
            namespace,
            name,
            attributes,
        } = document.kind(node).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "focus target must be an element"));
        };
        if *namespace != lumen_html::Namespace::Html {
            return Ok(false);
        }
        let local_name = lumen_html::forms::html_element_local_name(document, node).unwrap_or(name);
        let focusable = matches!(
            local_name,
            "input" | "button" | "textarea" | "select" | "summary"
        ) || attributes.iter().any(|(attribute, _)| attribute == "tabindex")
            || (local_name == "dialog"
                && document
                    .get_attribute_ns_ref(node, None, "open")
                    .map_err(dom_error)?
                    .is_some())
            || (local_name == "a" || local_name == "area")
                && document
                    .get_attribute_ns_ref(node, None, "href")
                    .map_err(dom_error)?
                    .is_some()
            || document
                .get_attribute_ns_ref(node, None, "contenteditable")
                .map_err(dom_error)?
                .is_some_and(|value| {
                    value.is_empty()
                        || value.eq_ignore_ascii_case("true")
                        || value.eq_ignore_ascii_case("plaintext-only")
                });
        if !focusable {
            return Ok(false);
        }
        let mut ancestor = node;
        while ancestor != document.root() {
            let Some(parent) = document
                .shadow_including_parent(ancestor)
                .map_err(dom_error)?
            else {
                return Ok(false);
            };
            ancestor = parent;
        }
        Ok(true)
    }

    fn focus_with_cause(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: Option<NodeId>,
        pointer_cause: bool,
    ) -> OpResult<()> {
        if let Some(node) = node {
            if !self.focusable_node(node)? {
                return Ok(());
            }
        }
        let old = self.focused_node();
        let old_was_visible = old.is_some_and(|old| self.focus_visible.get() == Some(old));
        if old == node {
            if let Some(node) = node {
                if pointer_cause {
                    self.focus_visible
                        .set(self.focus_always_visible(node).then_some(node));
                    self.publish_interaction_state();
                }
            }
            return Ok(());
        }
        self.focused.set(None);
        self.focus_visible.set(None);
        self.publish_interaction_state();
        let previous = old.map_or(Value::Null, |old| self.wrap(ctx, old));
        let next_value = node.map_or(Value::Null, |node| self.wrap(ctx, node));
        if let Some(old) = old {
            self.dispatch_user_agent(
                ctx,
                old,
                "blur",
                false,
                false,
                &[("relatedTarget", next_value.clone())],
            )?;
            self.dispatch_user_agent(
                ctx,
                old,
                "focusout",
                true,
                false,
                &[("relatedTarget", next_value)],
            )?;
        }
        if self.focused.get().is_some() {
            return Ok(());
        }
        self.focused.set(node);
        self.focus_visible.set(node.filter(|node| {
            self.focus_always_visible(*node)
                || (!pointer_cause && (self.keyboard_modality.get() || old_was_visible))
        }));
        self.publish_interaction_state();
        if let Some(node) = node {
            if self.focused_node() != Some(node) {
                self.focused.set(None);
                return Ok(());
            }
            self.dispatch_user_agent(
                ctx,
                node,
                "focus",
                false,
                false,
                &[("relatedTarget", previous.clone())],
            )?;
            if self.focused_node() == Some(node) {
                self.dispatch_user_agent(
                    ctx,
                    node,
                    "focusin",
                    true,
                    false,
                    &[("relatedTarget", previous)],
                )?;
            }
        }
        Ok(())
    }
    /// Dispatch an input event through the DOM ancestry. `false` means canceled.
    pub fn dispatch(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        kind: &str,
        bubbles: bool,
        cancelable: bool,
        properties: &[(&str, Value)],
    ) -> lumen::embed::OpResult<bool> {
        self.dispatch_with_trust(ctx, node, kind, bubbles, cancelable, properties, false)
    }

    /// Deliver a user-agent event to its actual DOM target. Re-dispatching the
    /// same event from script clears its trust through EventTarget.dispatchEvent.
    pub fn dispatch_user_agent(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        kind: &str,
        bubbles: bool,
        cancelable: bool,
        properties: &[(&str, Value)],
    ) -> OpResult<bool> {
        self.dispatch_with_trust(ctx, node, kind, bubbles, cancelable, properties, true)
    }

    /// Dispatch a native KeyboardEvent for host key input without running its
    /// default action. The caller applies the default after keydown and any
    /// legacy keypress cancellation point have completed.
    pub(crate) fn dispatch_user_agent_keyboard_event(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        kind: &str,
        properties: &[(&str, Value)],
    ) -> OpResult<bool> {
        self.session
            .borrow()
            .document()
            .kind(node)
            .map_err(|_| OpError::new("InvalidStateError", "event target no longer exists"))?;
        let target = self.wrap(ctx, node);
        let options = ctx.new_object_with_proto(&Value::Null);
        for (name, value) in [
            ("bubbles", Value::Bool(true)),
            ("cancelable", Value::Bool(true)),
            ("composed", Value::Bool(true)),
        ] {
            ctx.set_member(&options, name, value)
                .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        }
        let window = self
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade);
        if let Some(window) = window {
            ctx.set_member(&options, "view", window)
                .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        }
        for (name, value) in properties {
            ctx.set_member(&options, name, value.clone())
                .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
        }
        let event = ui_events::user_agent_keyboard_event(ctx, kind, options)?;
        self.dispatch_event_to_target(ctx, node, target, event, true)
    }

    fn dispatch_with_trust(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        kind: &str,
        bubbles: bool,
        cancelable: bool,
        properties: &[(&str, Value)],
        trusted: bool,
    ) -> OpResult<bool> {
        self.session.borrow().document().kind(node).map_err(|_| {
            lumen::embed::OpError::new("InvalidStateError", "event target no longer exists")
        })?;
        let value = self.wrap(ctx, node);
        let options = ctx.new_object_with_proto(&Value::Null);
        ctx.set_member(&options, "bubbles", Value::Bool(bubbles))
            .map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        ctx.set_member(&options, "cancelable", Value::Bool(cancelable))
            .map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        let composed = matches!(
            kind,
            "click"
                | "keydown"
                | "keyup"
                | "input"
                | "beforeinput"
                | "submit"
                | "paste"
                | "compositionstart"
                | "compositionupdate"
                | "compositionend"
                | "focus"
                | "blur"
                | "focusin"
                | "focusout"
        ) || kind.starts_with("pointer")
            || kind.starts_with("mouse");
        ctx.set_member(&options, "composed", Value::Bool(composed))
            .map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        if trusted && kind == "submit" {
            let submitter = properties
                .iter()
                .find(|(name, _)| *name == "submitter")
                .map(|(_, value)| value.clone());
            let event = events::DomSubmitEvent::for_user_agent(ctx, kind, submitter)?;
            return self.dispatch_event_to_target(ctx, node, value, event, true);
        }
        if trusted && kind == "formdata" {
            let form_data = properties
                .iter()
                .find(|(name, _)| *name == "formData")
                .map(|(_, value)| value.clone())
                .unwrap_or(Value::Null);
            let event = events::DomFormDataEvent::for_user_agent(ctx, kind, form_data)?;
            return self.dispatch_event_to_target(ctx, node, value, event, true);
        }
        if trusted && matches!(kind, "focus" | "blur" | "focusin" | "focusout") {
            let view = self
                .window_wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
                .unwrap_or(Value::Null);
            ctx.set_member(&options, "view", view)
                .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
            for (name, property) in properties {
                ctx.set_member(&options, name, property.clone())
                    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
            }
            let event = ui_events::user_agent_focus_event(ctx, kind, options)?;
            return self.dispatch_event_to_target(ctx, node, value, event, true);
        }
        if matches!(kind, "beforeinput" | "input")
            && properties.iter().any(|(name, _)| *name == "inputType")
        {
            if trusted {
                let view = self
                    .window_wrapper
                    .borrow()
                    .as_ref()
                    .and_then(WeakValue::upgrade)
                    .unwrap_or(Value::Null);
                ctx.set_member(&options, "view", view)
                    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
            }
            for (name, property) in properties {
                ctx.set_member(&options, name, property.clone())
                    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
            }
            let event = ui_events::input_event(ctx, kind, options)?;
            return self.dispatch_event_to_target(ctx, node, value, event, trusted);
        }
        if kind.starts_with("pointer") || kind.starts_with("mouse") {
            for (name, property) in properties {
                ctx.set_member(&options, name, property.clone())
                    .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
            }
            if let Some(event) = ui_events::host_pointer_or_mouse_event(ctx, kind, options.clone())? {
                return self.dispatch_event_to_target(ctx, node, value, event, trusted);
            }
        }
        let event = DomEvent::new(ctx, kind, Some(options))?;
        if let Some((_, related)) = properties.iter().find(|(name, _)| *name == "relatedTarget") {
            event.set_related_target(related.clone());
        }
        let event_number = |name: &str| {
            properties
                .iter()
                .find(|(property, _)| *property == name)
                .and_then(|(_, value)| match value {
                    Value::Num(value) => Some(*value),
                    _ => None,
                })
                .unwrap_or(0.0)
        };
        event.set_movement(event_number("movementX"), event_number("movementY"));
        let event = ctx.new_instance(event);
        for (name, property) in properties {
            if !matches!(*name, "relatedTarget" | "movementX" | "movementY") {
                ctx.set_member(&event, name, property.clone())
                    .map_err(|_| {
                        lumen::embed::OpError::new(
                            "TypeError",
                            "event property initialization failed",
                        )
                    })?;
            }
        }
        let allowed = self.dispatch_event_to_target(ctx, node, value, event, trusted)?;
        if allowed && kind == "keydown" && self.focused_node() == Some(node) {
            self.edit_control_key(ctx, node, properties)?;
        }
        Ok(allowed)
    }

    fn dispatch_event_to_target(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        target: Value,
        event: Value,
        trusted: bool,
    ) -> OpResult<bool> {
        let target_data = self
            .targets
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
            .ok_or_else(|| OpError::new("InvalidStateError", "event target no longer exists"))?;
        let event = lumen::embed::JsObject::from_value(event).expect("event object");
        let result = if trusted {
            events::dispatch_user_agent_event(ctx, lumen_bind::This(target), event)
        } else {
            DomEventTarget::dispatch_event(ctx, lumen_bind::This(target), event)
        };
        drop(target_data);
        result
    }

    pub fn session_handle(&self) -> Rc<RefCell<RenderSession>> {
        self.session.clone()
    }

    /// Dispatch a user-agent event through the native Window target. The
    /// embedder chooses the task and lifecycle point at which this runs.
    pub fn dispatch_window_user_agent(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        kind: &str,
        bubbles: bool,
        cancelable: bool,
    ) -> OpResult<bool> {
        let window = self
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window is unavailable"))?;
        let options = Value::Obj(ctx.new_object());
        ctx.member_set(&options, "bubbles", Value::Bool(bubbles))
            .map_err(OpError::thrown)?;
        ctx.member_set(&options, "cancelable", Value::Bool(cancelable))
            .map_err(OpError::thrown)?;
        let event = DomEvent::new(ctx, kind, Some(options))?;
        let event = lumen::embed::JsObject::from_value(ctx.new_instance(event))
            .expect("native Event object");
        if kind == "load" {
            let document = self.document_value(ctx);
            events::dispatch_user_agent_event_with_target(
                ctx,
                lumen_bind::This(window),
                event,
                document,
            )
        } else {
            events::dispatch_user_agent_event(ctx, lumen_bind::This(window), event)
        }
    }

    /// Dispatch a collected browser rejection notification to this realm's
    /// Window. The embedder queues this call on its existing user-agent task
    /// queue after the promise checkpoint. `false` means default was prevented.
    pub fn dispatch_promise_rejection(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        kind: &str,
        promise: Value,
        reason: Value,
    ) -> OpResult<bool> {
        if !matches!(kind, "unhandledrejection" | "rejectionhandled") {
            return Err(OpError::type_error("invalid promise rejection event type"));
        }
        let window = self
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window is unavailable"))?;
        let event = events::DomPromiseRejectionEvent::for_user_agent(ctx, kind, promise, reason)?;
        let event = lumen::embed::JsObject::from_value(ctx.new_instance(event))
            .expect("native PromiseRejectionEvent object");
        events::dispatch_user_agent_event(ctx, lumen_bind::This(window), event)
    }

    pub fn with_session<R>(&self, work: impl FnOnce(&mut RenderSession) -> R) -> R {
        if !self.detached.borrow().is_empty() {
            self.reap_detached(std::iter::empty());
        }
        let mut session = self.session.borrow_mut();
        session.synchronize_form_state();
        work(&mut session)
    }

    pub fn wrapper_count(&self) -> usize {
        self.wrappers.borrow().len()
    }

    fn prepare_native_identity_epoch(&self, epoch: u64) {
        if self.identity_trace_epoch.get() != epoch {
            self.identity_trace_nodes.borrow_mut().clear();
            self.identity_trace_epoch.set(epoch);
        }
    }

    fn native_identity_seen(&self, epoch: u64, id: NodeId) -> bool {
        self.prepare_native_identity_epoch(epoch);
        self.identity_trace_nodes.borrow().contains(&id)
    }

    fn mark_native_identity(&self, epoch: u64, id: NodeId) -> bool {
        self.prepare_native_identity_epoch(epoch);
        let mut nodes = self.identity_trace_nodes.borrow_mut();
        if nodes.contains(&id) {
            return false;
        }
        // On allocation failure correctness is preserved by allowing a later
        // callback to repeat tracing rather than dropping a native edge.
        if nodes.try_reserve(1).is_ok() {
            nodes.insert(id);
        }
        true
    }

    fn trace_native_identity_component(
        &self,
        epoch: u64,
        root: NodeId,
        visit: &mut dyn FnMut(&Value),
    ) {
        if self.native_identity_seen(epoch, root) {
            return;
        }
        let session = self.session.borrow();
        let document = session.document();
        let document_root = document.root();
        let mut cursor = root;
        for _ in 0..=document.node_count() {
            if cursor != document_root {
                self.trace_cached_native_wrapper(epoch, cursor, visit);
            }
            // Attr nodes are sparse sidecar entries rather than tree children.
            // Visit only already-materialized identities; GC must not create a
            // wrapper or materialize an attribute.
            if matches!(document.kind(cursor), Ok(NodeKind::Element { .. })) {
                if let Some(attributes) = document.materialized_attribute_nodes(cursor) {
                    for &(_, attribute) in attributes {
                        self.trace_cached_native_wrapper(epoch, attribute, visit);
                    }
                }
            }
            match selector::next_native_identity_descendant(document, root, cursor) {
                Ok(Some(next)) => cursor = next,
                Ok(None) | Err(_) => break,
            }
        }
        // A wrapped root was marked by the walk above. Record an unwrapped
        // root too, so later callbacks from nodes in this component skip the
        // parent walk before the next collection starts.
        self.mark_native_identity(epoch, root);
    }

    fn trace_cached_native_wrapper(
        &self,
        epoch: u64,
        id: NodeId,
        visit: &mut dyn FnMut(&Value),
    ) {
        let value = self
            .wrappers
            .borrow()
            .get(&id)
            .and_then(WeakValue::upgrade);
        if let Some(value) = value {
            if self.mark_native_identity(epoch, id) {
                visit(&value);
            }
        }
    }

    fn trace_document_wrapper(&self, epoch: u64, visit: &mut dyn FnMut(&Value)) {
        let document_root = self.session.borrow().document().root();
        self.trace_native_identity_component(epoch, document_root, visit);
        let value = self
            .document_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade);
        if let Some(value) = value {
            // Duplicate visits are harmless (the GC marker is idempotent).
            // The NodeId is already in the shared seen set because the
            // document component was walked before this ownerDocument edge.
            visit(&value);
        }
    }

    /// Reports whether a DOM node currently has a live JavaScript wrapper.
    ///
    /// This is intentionally a diagnostic for native profile tests: looking up a
    /// node through the public DOM API would itself create the wrapper being
    /// measured.
    pub fn has_live_wrapper(&self, node: NodeId) -> bool {
        self.wrappers
            .borrow()
            .get(&node)
            .is_some_and(|wrapper| wrapper.upgrade().is_some())
    }

    fn reap_detached(&self, added: impl IntoIterator<Item = NodeId>) {
        let _html_allocations = enter_html_allocation_category();
        let mut pending = self.detached.borrow_mut();
        pending.extend(added);
        let mut session = self.session.borrow_mut();
        let document = session.document_mut();
        let wrappers = self.wrappers.borrow();
        let current_script = self.current_script.get();
        pending.retain(|&root| {
            if document.kind(root).is_err() || document.parent(root).ok().flatten().is_some() {
                return false;
            }
            let mut ancestor = current_script;
            while let Some(id) = ancestor {
                if id == root {
                    // A parser may execute a script before its wrapper has ever been requested.
                    // Keep a detached currentScript alive until script evaluation finishes.
                    return true;
                }
                ancestor = document.parent(id).ok().flatten();
            }
            let mut cursor = Some(root);
            let mut tree_root = root;
            let mut contents = Vec::new();
            let mut live = false;
            while let Some(id) = cursor {
                if wrappers
                    .get(&id)
                    .is_some_and(|value| value.upgrade().is_some())
                    || self.retained_nodes.borrow().contains_key(&id)
                {
                    live = true;
                    break;
                }
                // Attr nodes are lazily materialized in the owner's sidecar, not
                // linked into the ordinary child tree. A retained Attr therefore
                // keeps a detached owner Element alive just like a descendant Node.
                if document
                    .materialized_attribute_nodes(id)
                    .is_some_and(|attributes| {
                        attributes.iter().any(|&(_, attribute)| {
                            wrappers
                                .get(&attribute)
                                .is_some_and(|value| value.upgrade().is_some())
                                || self.retained_nodes.borrow().contains_key(&attribute)
                        })
                    })
                {
                    live = true;
                    break;
                }
                if let Ok(Some(content)) = document.template_content(id) {
                    contents.push(content);
                }
                if let Ok(Some(content)) = document.shadow_root(id) {
                    contents.push(content);
                }
                cursor = next_descendant(document, tree_root, id).ok().flatten();
                if cursor.is_none() {
                    if let Some(content) = contents.pop() {
                        tree_root = content;
                        cursor = Some(content);
                    }
                }
            }
            if live {
                true
            } else {
                let _ = document.destroy_subtree(root);
                false
            }
        });
        self.targets
            .borrow_mut()
            .retain(|id, target| document.kind(*id).is_ok() && target.strong_count() > 0);
        self.selections
            .borrow_mut()
            .retain(|id, _| document.kind(*id).is_ok());
        forms::reap(&mut self.forms.borrow_mut(), document);

        // The shared document clears interaction anchors when a subtree is
        // detached. Keep the realm's sparse publisher in sync as well, so a
        // retained node that is later reinserted does not regain old state.
        let focused = self
            .focused
            .get()
            .filter(|node| connected_interaction_element(document, *node));
        let focus_visible = self
            .focus_visible
            .get()
            .filter(|node| Some(*node) == focused);
        let hover = self
            .hover_target
            .get()
            .filter(|node| connected_interaction_element(document, *node));
        let active_targets = self.active_targets.get().map(|node| {
            node.filter(|node| connected_interaction_element(document, *node))
        });
        self.focused.set(focused);
        self.focus_visible.set(focus_visible);
        self.hover_target.set(hover);
        self.active_targets.set(active_targets);
        document.set_interaction_state(lumen_html::interaction::InteractionState {
            focused,
            focus_visible,
            hover,
            active: active_targets,
        });
    }

    fn migrate_adopted_state(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        target: &Rc<DomRealm>,
        mapping: &[(NodeId, NodeId)],
    ) -> OpResult<()> {
        for &(old, new) in mapping {
            let wrapper = self
                .wrappers
                .borrow_mut()
                .remove(&old)
                .and_then(|weak| weak.upgrade());
            if let Some(wrapper) = wrapper {
                let mut collections = ctx.with_instance_mut::<DomNode, _>(&wrapper, |node| {
                    node.base.rebind_node(target, new);
                    node.realm = target.clone();
                    node.id = new;
                    core::mem::take(&mut *node.collections.borrow_mut())
                })?;
                ctx.set_native_identity_owner::<DomNode>(&wrapper)?;
                for weak in collections.values() {
                    let Some(collection) = weak.upgrade() else {
                        continue;
                    };
                    if ctx
                        .with_instance_mut::<DomNodeList, _>(&collection, |list| {
                            list.adopt_nodes(target.clone(), mapping)
                        })
                        .is_ok()
                    {
                        continue;
                    }
                    if ctx
                        .with_instance_mut::<DomTokenList, _>(&collection, |list| {
                            list.adopt_node(target.clone(), new)
                        })
                        .is_ok()
                    {
                        continue;
                    }
                    if ctx
                        .with_instance_mut::<cssom::DomStylePropertyMapReadOnly, _>(
                            &collection,
                            |map| {
                                map.realm = target.clone();
                                map.node = new;
                            },
                        )
                        .is_ok()
                    {
                        continue;
                    }
                    let _ = ctx.with_instance_mut::<DomStyle, _>(&collection, |style| {
                        style.adopt_node(target.clone(), new)
                    });
                }
                ctx.with_instance_mut::<DomNode, _>(&wrapper, |node| {
                    node.collections.borrow_mut().extend(collections)
                })?;
                target.wrappers.borrow_mut().insert(
                    new,
                    ctx.weak_value(&wrapper)
                        .expect("adopted node wrapper is an object"),
                );
            }
            if let Some(count) = self.retained_nodes.borrow_mut().remove(&old) {
                *target.retained_nodes.borrow_mut().entry(new).or_default() += count;
            }
            let script_retentions = self
                .script_retentions
                .borrow_mut()
                .remove(&old)
                .unwrap_or_default();
            for weak in script_retentions {
                let Some(retention) = weak.upgrade() else {
                    continue;
                };
                {
                    let mut retention = retention.borrow_mut();
                    retention.owner = target.clone();
                    retention.node = new;
                }
                target
                    .script_retentions
                    .borrow_mut()
                    .entry(new)
                    .or_default()
                    .push(Rc::downgrade(&retention));
            }
            self.adopted_nodes
                .borrow_mut()
                .insert(old, (Rc::downgrade(target), new));
            if let Some(selection) = self.selections.borrow_mut().remove(&old) {
                target.selections.borrow_mut().insert(new, selection);
            }
        }
        self.scripts
            .borrow_mut()
            .adopt_nodes_into(&mut target.scripts.borrow_mut(), mapping);
        forms::adopt_nodes_into(
            &mut self.forms.borrow_mut(),
            &mut target.forms.borrow_mut(),
            mapping,
        );
        self.ranges
            .adopt_nodes(self, &target.ranges, target, mapping);
        if let Some(selection) = self.selection.borrow().as_ref() {
            selection.adopt_nodes(mapping);
        }
        animations::adopt_nodes(ctx, self, target, mapping)?;
        self.canvases
            .adopt_nodes_into(ctx, &target.canvases, target, mapping)?;
        self.images.adopt_nodes_into(&target.images, mapping);
        self.media.borrow_mut().adopt_nodes_into(&mut target.media.borrow_mut(), mapping);
        self.iterators
            .adopt_nodes(self, target, &target.iterators, mapping);
        self.tree_walkers
            .adopt_nodes(self, target, &target.tree_walkers, mapping);
        observers::adopt_nodes(ctx, self, target, mapping);
        dialog_popover::adopt_details_tasks(ctx, self, target, mapping)?;
        Ok(())
    }

    fn adopt_node_from(
        target: &Rc<Self>,
        ctx: &mut Ctx,
        source: &Rc<Self>,
        source_id: NodeId,
    ) -> OpResult<NodeId> {
        Self::adopt_node_from_with_validation(target, ctx, source, source_id, |_, _| Ok(()))
    }

    fn adopt_node_from_with_validation(
        target: &Rc<Self>,
        ctx: &mut Ctx,
        source: &Rc<Self>,
        source_id: NodeId,
        validate: impl FnOnce(&lumen_html::Document, &lumen_html::Document) -> Result<(), Error>,
    ) -> OpResult<NodeId> {
        {
            let session = source.session.borrow();
            let document = session.document();
            if matches!(
                document.kind(source_id).map_err(dom_error)?,
                NodeKind::Document
            ) || document
                .shadow_host(source_id)
                .map_err(dom_error)?
                .is_some()
            {
                return Err(OpError::new(
                    "NotSupportedError",
                    "documents and shadow roots cannot be adopted",
                ));
            }
        }

        // Adoption clears focus even when the subtree is detached. The focused
        // id belongs to the source arena, so clear it before moving either a
        // same-document node or a cross-document subtree.
        let focused_in_subtree = source.focused.get().is_some_and(|focused| {
            let session = source.session.borrow();
            let document = session.document();
            let mut current = Some(focused);
            while let Some(node) = current {
                if node == source_id {
                    return true;
                }
                current = document.shadow_including_parent(node).ok().flatten();
            }
            false
        });
        if focused_in_subtree {
            let focused = source.focused.get().expect("checked focused node");
            source.focused.set(None);
            let related_target = Value::Null;
            source.dispatch(
                ctx,
                focused,
                "blur",
                false,
                false,
                &[("relatedTarget", related_target.clone())],
            )?;
            source.dispatch(
                ctx,
                focused,
                "focusout",
                true,
                false,
                &[("relatedTarget", related_target)],
            )?;
            // Focus notifications can run script that changes the target
            // reference or donor subtree. Recheck before moving any nodes.
            let target_session = target.session.borrow();
            let source_session = source.session.borrow();
            validate(target_session.document(), source_session.document()).map_err(dom_error)?;
        }

        if Rc::ptr_eq(source, target) {
            source
                .session
                .borrow_mut()
                .document_mut()
                .remove(source_id)
                .map_err(dom_error)?;
            return Ok(source_id);
        }

        let (adopted_root, mapping) = {
            let mut source_session = source.session.borrow_mut();
            let mut target_session = target.session.borrow_mut();
            target_session
                .document_mut()
                .adopt_subtree_from(source_session.document_mut(), source_id)
                .map_err(dom_error)?
        };
        source.migrate_adopted_state(ctx, target, &mapping)?;
        custom_elements::adopt_nodes(ctx, source, target, &mapping)?;
        presentation::adopt_nodes(ctx, source, &mapping)?;
        animations::adopt_nodes(ctx, source, target, &mapping)?;
        event_content_handlers::initialize_subtree(ctx, target, adopted_root)?;
        Ok(adopted_root)
    }

    fn wrap(self: &Rc<Self>, ctx: &mut Ctx, id: NodeId) -> Value {
        let _html_allocations = enter_html_allocation_category();
        if id == self.session.borrow().document().root() {
            return self.document_value(ctx);
        }
        if let Some(value) = self.wrappers.borrow().get(&id).and_then(WeakValue::upgrade) {
            return value;
        }
        let node = DomNode {
            base: DomEventTarget::node(self, id),
            realm: self.clone(),
            id,
            collections: RefCell::new(HashMap::new()),
        };
        let value = match self.session.borrow().document().kind(id) {
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "html" => {
                ctx.cached_instance(id, || DomHtmlHtmlElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "div" => {
                ctx.cached_instance(id, || DomHtmlDivElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "br" => {
                ctx.cached_instance(id, || DomHtmlBrElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "head" => {
                ctx.cached_instance(id, || DomHtmlHeadElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "a" => {
                ctx.cached_instance(id, || hyperlinks::DomAnchorElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "area" => {
                ctx.cached_instance(id, || hyperlinks::DomAreaElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "body" => {
                ctx.cached_instance(id, || DomHtmlBodyElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "title" => {
                ctx.cached_instance(id, || DomHtmlTitleElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::xml::split_qname(name.as_str())
                .is_some_and(|(_, local_name)| local_name == "base") =>
            {
                ctx.cached_instance(id, || DomHtmlBaseElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "link" => {
                ctx.cached_instance(id, || DomHtmlLinkElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::xml::split_qname(name.as_str())
                .is_some_and(|(_, local)| local == "script") =>
            {
                ctx.cached_instance(id, || DomHtmlScriptElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "img" => {
                ctx.cached_instance(id, || DomHtmlImageElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "audio" => {
                self.media.borrow_mut().register_detached(id);
                ctx.cached_instance(id, || media::DomHtmlAudioElement {
                    base: media::DomHtmlMediaElement {
                        base: DomHtmlElement {
                            base: DomElement { base: node },
                        },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "video" => {
                self.media.borrow_mut().register_detached(id);
                ctx.cached_instance(id, || media::DomHtmlVideoElement {
                    base: media::DomHtmlMediaElement {
                        base: DomHtmlElement {
                            base: DomElement { base: node },
                        },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "canvas" => ctx.cached_instance(id, || {
                canvas::DomCanvasElement::from_node(DomHtmlElement {
                    base: DomElement { base: node },
                })
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "form" => {
                ctx.cached_instance(id, || DomFormElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "details" => {
                ctx.cached_instance(id, || DomDetailsElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "style" => {
                ctx.cached_instance(id, || DomStyleElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "template" => {
                ctx.cached_instance(id, || DomTemplateElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "iframe" => {
                ctx.cached_instance(id, || DomIFrameElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "input" => {
                ctx.cached_instance(id, || DomInputElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "select" => {
                ctx.cached_instance(id, || DomSelectElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "option" => {
                ctx.cached_instance(id, || DomOptionElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "textarea" => {
                ctx.cached_instance(id, || DomTextAreaElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if lumen_html::svg::local_name(name) == "slot" => {
                ctx.cached_instance(id, || DomSlotElement {
                    base: DomHtmlElement {
                        base: DomElement { base: node },
                    },
                })
            }
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) => {
                match html_interfaces::wrap_known(ctx, id, node, lumen_html::svg::local_name(name))
                {
                    Ok(value) => value,
                    Err(node) => match lumen_html::html::classify_html_element_name(
                        lumen_html::svg::local_name(name),
                    ) {
                        lumen_html::html::HtmlElementNameKind::BuiltIn
                        | lumen_html::html::HtmlElementNameKind::Custom => {
                            ctx.cached_instance(id, || DomHtmlElement {
                                base: DomElement { base: node },
                            })
                        }
                        lumen_html::html::HtmlElementNameKind::Unknown => {
                            html_interfaces::wrap_unknown(ctx, id, node)
                        }
                    },
                }
            }
            Ok(NodeKind::Element { .. }) => ctx.cached_instance(id, || DomElement { base: node }),
            Ok(NodeKind::Text(_)) => ctx.cached_instance(id, || DomText {
                base: DomCharacterData { base: node },
            }),
            Ok(NodeKind::CData(_)) => ctx.cached_instance(id, || DomCDataSection {
                base: DomText {
                    base: DomCharacterData { base: node },
                },
            }),
            Ok(NodeKind::Attribute { .. }) => {
                ctx.cached_instance(id, || attributes::DomAttr { base: node })
            }
            Ok(NodeKind::ProcessingInstruction { .. }) => {
                ctx.cached_instance(id, || DomProcessingInstruction {
                    base: DomCharacterData { base: node },
                })
            }
            Ok(NodeKind::DocumentType(_)) => {
                ctx.cached_instance(id, || DomDocumentType { base: node })
            }
            Ok(NodeKind::Comment(_)) => ctx.cached_instance(id, || DomComment {
                base: DomCharacterData { base: node },
            }),
            Ok(NodeKind::DocumentFragment)
                if self
                    .session
                    .borrow()
                    .document()
                    .shadow_host(id)
                    .ok()
                    .flatten()
                    .is_some() =>
            {
                ctx.cached_instance(id, || DomShadowRoot {
                    base: DomDocumentFragment { base: node },
                })
            }
            Ok(NodeKind::DocumentFragment) => {
                ctx.cached_instance(id, || DomDocumentFragment { base: node })
            }
            _ => ctx.cached_instance(id, || node),
        };
        ctx.set_native_identity_owner::<DomNode>(&value)
            .ok()
            .expect("DOM node wrapper has its native identity owner");
        let weak = ctx.weak_value(&value).expect("native node is an object");
        let mut wrappers = self.wrappers.borrow_mut();
        wrappers.insert(id, weak);
        if wrappers.len() >= self.sweep_at.get().max(256) {
            wrappers.retain(|_, value| value.upgrade().is_some());
            self.targets
                .borrow_mut()
                .retain(|_, target| target.strong_count() > 0);
            self.sweep_at.set(wrappers.len().saturating_mul(2).max(256));
        }
        value
    }

    fn wrap_option(self: &Rc<Self>, ctx: &mut Ctx, id: Option<NodeId>) -> Value {
        id.map_or(Value::Null, |id| self.wrap(ctx, id))
    }
}

fn dom_error(error: Error) -> OpError {
    let name = match error {
        Error::InvalidNode => "NotFoundError",
        Error::NotFound => "NotFoundError",
        Error::InUseAttribute => "InUseAttributeError",
        Error::Hierarchy => "HierarchyRequestError",
        Error::LimitExceeded => "QuotaExceededError",
        Error::IndexSize => "IndexSizeError",
        Error::WrongKind | Error::UnsupportedDoctype => "NotSupportedError",
    };
    OpError::new(name, format!("DOM operation failed: {error:?}"))
}

fn html_parser_error(error: html::ParseError) -> OpError {
    let name = if error.message.contains("limit exceeded") || error.message.contains("too large") {
        "QuotaExceededError"
    } else {
        "InvalidStateError"
    };
    OpError::new(name, error.message)
}

fn adjacent_dom_insertion_point(
    document: &lumen_html::Document,
    element: NodeId,
    position: &str,
) -> OpResult<Option<(NodeId, Option<NodeId>)>> {
    if position.eq_ignore_ascii_case("beforebegin") || position.eq_ignore_ascii_case("afterend") {
        let Some(parent) = document.parent(element).map_err(dom_error)? else {
            return Ok(None);
        };
        let before = if position.eq_ignore_ascii_case("beforebegin") {
            Some(element)
        } else {
            document.next_sibling(element).map_err(dom_error)?
        };
        Ok(Some((parent, before)))
    } else if position.eq_ignore_ascii_case("afterbegin") {
        Ok(Some((
            element,
            document.first_child(element).map_err(dom_error)?,
        )))
    } else if position.eq_ignore_ascii_case("beforeend") {
        Ok(Some((element, None)))
    } else {
        Err(OpError::new(
            "SyntaxError",
            "invalid adjacent insertion position",
        ))
    }
}

fn parse_dom_markup_fragment(
    document: &mut lumen_html::Document,
    context: NodeId,
    value: &str,
) -> OpResult<NodeId> {
    if document.is_html_document() {
        html::parse_fragment_in(document, context, value).map_err(html_parser_error)
    } else {
        lumen_html::xml::parse_fragment_in(document, context, value).map_err(|error| {
            let name = if error.message.contains("limit exceeded")
                || error.message.contains("too large")
            {
                "QuotaExceededError"
            } else {
                "SyntaxError"
            };
            OpError::new(name, error.message)
        })
    }
}

fn dom_markup_error(ctx: &mut Ctx, error: OpError) -> OpError {
    if error.class() == "SyntaxError" {
        error_reporting::dom_exception(ctx, "SyntaxError", error.message())
    } else {
        error
    }
}

fn insert_dom_node(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    parent: NodeId,
    child: Value,
    before: Value,
) -> OpResult<Value> {
    // Extract identity before adoption. Migrating an existing wrapper may
    // rebind its native payload, so no projection can remain borrowed.
    let (source, source_id) =
        ctx.with_instance::<DomNode, _>(&child, |node| (node.realm.clone(), node.id))?;
    let reference = if matches!(before, Value::Null | Value::Undefined) {
        None
    } else {
        Some(ctx.with_instance::<DomNode, _>(&before, |node| node.id)?)
    };
    let (source_has_options, preferred_source_option) =
        forms::option_subtree_selection(&source, source_id)?;
    let (source_select, source_is_fragment) = {
        let session = source.session.borrow();
        let document = session.document();
        let old_select = if source_has_options {
            match document.parent(source_id).map_err(dom_error)? {
                Some(parent) => lumen_html::forms::select_ancestor(document, parent)
                    .map_err(dom_error)?,
                None => None,
            }
        } else {
            None
        };
        let is_fragment = matches!(document.kind(source_id).map_err(dom_error)?, NodeKind::DocumentFragment);
        (old_select, is_fragment)
    };
    let target_select = {
        let session = realm.session.borrow();
        let document = session.document();
        if source_has_options {
            lumen_html::forms::select_ancestor(document, parent).map_err(dom_error)?
        } else {
            None
        }
    };

    // Pre-insertion validity must precede adoption, preserving the donor
    // document and ownerDocument on a hierarchy/reference failure.
    let id = if Rc::ptr_eq(realm, &source) {
        // The ordinary insertion already validates before mutation. Avoid
        // an extra fragment walk/allocation on this common path.
        source_id
    } else {
        {
            let target = realm.session.borrow();
            let source_session = source.session.borrow();
            target
                .document()
                .validate_insert_from(source_session.document(), parent, source_id, reference)
                .map_err(dom_error)?;
        }
        DomRealm::adopt_node_from_with_validation(
            realm,
            ctx,
            &source,
            source_id,
            |target, donor| target.validate_insert_from(donor, parent, source_id, reference),
        )?
    };
    realm
        .session
        .borrow_mut()
        .document_mut()
        .insert_before(parent, id, reference)
        .map_err(dom_error)?;
    if let Some(select) = source_select {
        forms::select_option_list_changed(&source, select)?;
    }
    if let Some(select) = target_select {
        if !Rc::ptr_eq(realm, &source) || source_select != Some(select) {
            let preferred = if Rc::ptr_eq(realm, &source) {
                preferred_source_option
            } else if preferred_source_option == Some(source_id) {
                Some(id)
            } else if source_has_options && !source_is_fragment {
                forms::selected_option_in_subtree(realm, id)?
            } else {
                None
            };
            forms::select_option_list_changed_with_preferred(realm, select, preferred)?;
        }
    }
    realm.invalidate_textarea_ancestor(parent);
    realm.flush_script_activations(ctx)?;
    Ok(child)
}

fn select_for_removed_option_subtree(
    realm: &DomRealm,
    subtree: NodeId,
    parent: Option<NodeId>,
) -> OpResult<Option<NodeId>> {
    let Some(parent) = parent else {
        return Ok(None);
    };
    let session = realm.session.borrow();
    let document = session.document();
    if !lumen_html::forms::subtree_contains_select_option(document, subtree).map_err(dom_error)? {
        return Ok(None);
    }
    lumen_html::forms::select_ancestor(document, parent)
        .map_err(dom_error)
}

#[derive(Default)]
struct SelectMutationSnapshot {
    source_selects: Vec<NodeId>,
    target_select: Option<NodeId>,
    preferred_option: Option<NodeId>,
}

fn capture_select_mutation(
    realm: &Rc<DomRealm>,
    parent: NodeId,
    inserted: &[NodeId],
    removed: &[NodeId],
) -> OpResult<SelectMutationSnapshot> {
    let mut affected = false;
    let mut source_selects = Vec::new();
    let mut preferred_option = None;
    for root in inserted.iter().copied() {
        let (has_options, selected) = forms::option_subtree_selection(realm, root)?;
        if !has_options {
            continue;
        }
        affected = true;
        if selected.is_some() {
            preferred_option = selected;
        }
        let source_select = {
            let session = realm.session.borrow();
            let document = session.document();
            document
                .parent(root)
                .map_err(dom_error)?
                .map(|source_parent| {
                    lumen_html::forms::select_ancestor(document, source_parent)
                })
                .transpose()
                .map_err(dom_error)?
                .flatten()
        };
        if let Some(source_select) = source_select {
            if !source_selects.contains(&source_select) {
                source_selects.push(source_select);
            }
        }
    }
    for root in removed.iter().copied() {
        let session = realm.session.borrow();
        if lumen_html::forms::subtree_contains_select_option(session.document(), root)
            .map_err(dom_error)?
        {
            affected = true;
            break;
        }
    }
    if !affected {
        return Ok(SelectMutationSnapshot::default());
    }
    let target_select = {
        let session = realm.session.borrow();
        lumen_html::forms::select_ancestor(session.document(), parent).map_err(dom_error)?
    };
    source_selects.retain(|select| Some(*select) != target_select);
    Ok(SelectMutationSnapshot {
        source_selects,
        target_select,
        preferred_option,
    })
}

fn apply_select_mutation(
    realm: &Rc<DomRealm>,
    mutation: SelectMutationSnapshot,
) -> OpResult<()> {
    for select in mutation.source_selects {
        forms::select_option_list_changed(realm, select)?;
    }
    if let Some(select) = mutation.target_select {
        forms::select_option_list_changed_with_preferred(
            realm,
            select,
            mutation.preferred_option,
        )?;
    }
    Ok(())
}

fn selector_error(ctx: &mut Ctx, error: selector::SelectorError) -> OpError {
    error_reporting::dom_exception(ctx, "SyntaxError", &format!("Invalid selector: {error:?}"))
}

fn tag_ns_collection_key(namespace_uri: Option<&str>, local_name: &str) -> String {
    match namespace_uri {
        Some(namespace_uri) => format!(
            "tag-ns:some:{}:{}:{}:{}",
            namespace_uri.len(),
            namespace_uri,
            local_name.len(),
            local_name
        ),
        None => format!("tag-ns:none:{}:{local_name}", local_name.len()),
    }
}

fn named_child(
    document: &lumen_html::Document,
    parent: NodeId,
    wanted: &str,
) -> Result<Option<NodeId>, Error> {
    let mut child = document.first_child(parent)?;
    while let Some(id) = child {
        if matches!(document.kind(id)?, NodeKind::Element { name, .. } if name == wanted) {
            return Ok(Some(id));
        }
        child = document.next_sibling(id)?;
    }
    Ok(None)
}

fn namespace_from_uri(namespace_uri: Option<&str>) -> Namespace {
    match namespace_uri.filter(|uri| !uri.is_empty()) {
        Some("http://www.w3.org/1999/xhtml") => Namespace::Html,
        Some("http://www.w3.org/2000/svg") => Namespace::Svg,
        Some("http://www.w3.org/1998/Math/MathML") => Namespace::MathMl,
        Some(uri) => Namespace::Other(Rc::from(uri)),
        None => Namespace::Other(Rc::from("")),
    }
}

fn namespace_for_qname(
    ctx: &mut Ctx,
    namespace_uri: Option<&str>,
    qualified_name: &str,
) -> OpResult<Namespace> {
    let Some((prefix, _local_name)) = lumen_html::xml::split_qname(qualified_name) else {
        return Err(error_reporting::dom_exception(
            ctx,
            "InvalidCharacterError",
            "qualified name is not a valid XML QName",
        ));
    };
    let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
    let xml_uri = "http://www.w3.org/XML/1998/namespace";
    let xmlns_uri = "http://www.w3.org/2000/xmlns/";
    if (!prefix.is_empty() && namespace_uri.is_none())
        || (prefix == "xml" && namespace_uri != Some(xml_uri))
        || ((qualified_name == "xmlns" || prefix == "xmlns") != (namespace_uri == Some(xmlns_uri)))
    {
        return Err(error_reporting::dom_exception(
            ctx,
            "NamespaceError",
            "qualified name prefix and namespace URI do not match",
        ));
    }
    Ok(namespace_from_uri(namespace_uri))
}

fn html_element(document: &mut lumen_html::Document, name: &str) -> Result<NodeId, Error> {
    document.create(NodeKind::Element {
        namespace: Namespace::Html,
        name: name.into(),
        attributes: Vec::new(),
    })
}

fn converted_dom_nodes(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    values: Vec<Value>,
) -> OpResult<(Vec<NodeId>, Vec<NodeId>)> {
    enum Pending {
        Node(NodeId),
        Text(String),
    }
    let mut pending = Vec::with_capacity(values.len());
    for value in values {
        // `instance_data` only finds an exact native type. DOM nodes are
        // almost always stored as a derived facade (Element, Text, ...), so
        // inspect the projected Node base instead or WebIDL Node-or-DOMString
        // methods silently stringify those nodes.
        let node = ctx.with_instance::<DomNode, _>(&value, |node| (node.realm.clone(), node.id));
        if let Ok((node_realm, id)) = node {
            if !Rc::ptr_eq(realm, &node_realm) {
                return Err(OpError::new(
                    "WrongDocumentError",
                    "nodes belong to different documents",
                ));
            }
            pending.push(Pending::Node(id));
        } else {
            pending.push(Pending::Text(
                ctx.coerce_string(&value)
                    .map_err(OpError::thrown)?
                    .to_string(),
            ));
        }
    }
    let mut nodes = Vec::with_capacity(pending.len());
    let mut generated = Vec::new();
    let mut session = realm.session.borrow_mut();
    for pending in pending {
        match pending {
            Pending::Node(id) => nodes.push(id),
            Pending::Text(text) => match session.document_mut().create(NodeKind::Text(text)) {
                Ok(id) => {
                    nodes.push(id);
                    generated.push(id);
                }
                Err(error) => {
                    for id in generated {
                        let _ = session.document_mut().destroy_subtree(id);
                    }
                    return Err(dom_error(error));
                }
            },
        }
    }
    Ok((nodes, generated))
}

fn discard_generated_dom_nodes(realm: &Rc<DomRealm>, nodes: &[NodeId]) {
    let mut session = realm.session.borrow_mut();
    for &id in nodes {
        let _ = session.document_mut().destroy_subtree(id);
    }
}

#[lumen_bind::class(name = "DOMImplementation", hint(js(webidl)))]
pub struct DomImplementation {
    realm: Rc<DomRealm>,
}

#[lumen_bind::methods]
impl DomImplementation {
    #[method(coerce)]
    fn has_feature(&self, #[default("")] _feature: &str, #[default("")] _version: &str) -> bool {
        true
    }

    #[method(name = "createHTMLDocument", coerce)]
    fn create_html_document(
        &self,
        ctx: &mut Ctx,
        #[default(Value::Undefined)] title: Value,
    ) -> OpResult<Value> {
        let title = if matches!(&title, Value::Undefined) {
            None
        } else {
            Some(
                ctx.coerce_string(&title)
                    .map_err(OpError::thrown)?
                    .to_string(),
            )
        };
        let controller = dialog_popover::DetailsController::prepare(ctx)?;
        let mut document = lumen_html::Document::new(100_000);
        controller.attach(&mut document);
        let root = document.root();
        let doctype = document
            .create(NodeKind::DocumentType("html".into()))
            .map_err(dom_error)?;
        document.append(root, doctype).map_err(dom_error)?;
        let html = html_element(&mut document, "html").map_err(dom_error)?;
        document.append(root, html).map_err(dom_error)?;
        let head = html_element(&mut document, "head").map_err(dom_error)?;
        document.append(html, head).map_err(dom_error)?;
        if let Some(title) = title {
            let title_node = html_element(&mut document, "title").map_err(dom_error)?;
            document.append(head, title_node).map_err(dom_error)?;
            let text = document.create(NodeKind::Text(title)).map_err(dom_error)?;
            document.append(title_node, text).map_err(dom_error)?;
        }
        let body = html_element(&mut document, "body").map_err(dom_error)?;
        document.append(html, body).map_err(dom_error)?;

        let realm = DomRealm::realm_from_document_with_metadata(document, "text/html", true, false);
        realm.set_document_url("about:blank");
        realm.set_document_origin(
            self.realm
                .document_origin()
                .unwrap_or_else(browsing_context::Origin::opaque),
        );
        controller.bind(ctx, &realm);
        Ok(realm.document_value(ctx))
    }

    #[method(coerce)]
    fn create_document_type(
        &self,
        ctx: &mut Ctx,
        qualified_name: &str,
        public_id: &str,
        system_id: &str,
    ) -> OpResult<Value> {
        if !lumen_html::xml::is_xml_name(qualified_name) {
            return Err(OpError::new(
                "InvalidCharacterError",
                "document type name is not a valid XML Name",
            ));
        }
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let id = document
            .create(NodeKind::DocumentType(qualified_name.into()))
            .map_err(dom_error)?;
        document
            .set_doctype_identifiers(id, public_id, system_id)
            .map_err(dom_error)?;
        drop(session);
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn create_document(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        qualified_name: Value,
        #[default(Value::Null)] doctype: Value,
    ) -> OpResult<Value> {
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        let qualified_name = if matches!(&qualified_name, Value::Null) {
            None
        } else {
            Some(
                ctx.coerce_string(&qualified_name)
                    .map_err(OpError::thrown)?
                    .to_string(),
            )
        };
        let element_namespace = match qualified_name.as_deref() {
            None | Some("") => None,
            Some(name) => Some(namespace_for_qname(ctx, namespace_uri, name)?),
        };
        let content_type = match namespace_uri {
            Some("http://www.w3.org/1999/xhtml") => "application/xhtml+xml",
            Some("http://www.w3.org/2000/svg") => "image/svg+xml",
            _ => "application/xml",
        };
        let controller = dialog_popover::DetailsController::prepare(ctx)?;
        let mut document = lumen_html::Document::new(100_000);
        controller.attach(&mut document);
        let realm = DomRealm::realm_from_document_with_interface(
            document,
            content_type,
            false,
            false,
            DocumentInterface::XmlDocument,
        );
        realm.set_document_url("about:blank");
        realm.set_document_origin(
            self.realm
                .document_origin()
                .unwrap_or_else(browsing_context::Origin::opaque),
        );

        if !matches!(&doctype, Value::Null | Value::Undefined) {
            let (source_realm, source_id) = ctx
                .with_instance::<DomNode, _>(&doctype, |node| (node.realm.clone(), node.id))
                .map_err(|_| OpError::new("TypeError", "doctype must be a DocumentType"))?;
            if !matches!(
                source_realm.session.borrow().document().kind(source_id),
                Ok(NodeKind::DocumentType(_))
            ) {
                return Err(OpError::new("TypeError", "doctype must be a DocumentType"));
            }
            let (adopted, mapping) = {
                let mut source = source_realm.session.borrow_mut();
                let mut target = realm.session.borrow_mut();
                target
                    .document_mut()
                    .adopt_subtree_from(source.document_mut(), source_id)
                    .map_err(dom_error)?
            };
            source_realm.migrate_adopted_state(ctx, &realm, &mapping)?;
            custom_elements::adopt_nodes(ctx, &source_realm, &realm, &mapping)?;
            presentation::adopt_nodes(ctx, &source_realm, &mapping)?;
            animations::adopt_nodes(ctx, &source_realm, &realm, &mapping)?;
            let root = realm.session.borrow().document().root();
            realm
                .session
                .borrow_mut()
                .document_mut()
                .append(root, adopted)
                .map_err(dom_error)?;
        }

        if let (Some(namespace), Some(qualified_name)) = (element_namespace, qualified_name) {
            let id = realm
                .session
                .borrow_mut()
                .document_mut()
                .create(NodeKind::Element {
                    namespace,
                    name: qualified_name.into(),
                    attributes: Vec::new(),
                })
                .map_err(dom_error)?;
            let root = realm.session.borrow().document().root();
            realm
                .session
                .borrow_mut()
                .document_mut()
                .append(root, id)
                .map_err(dom_error)?;
        }
        controller.bind(ctx, &realm);
        Ok(realm.document_value(ctx))
    }
}

fn element_by_id(document: &lumen_html::Document, wanted: &str) -> Result<Option<NodeId>, Error> {
    let root = document.root();
    element_by_id_in(document, root, wanted)
}
fn element_by_id_in(
    document: &lumen_html::Document,
    root: NodeId,
    wanted: &str,
) -> Result<Option<NodeId>, Error> {
    selector::get_element_by_id(document, root, wanted)
}

fn children(document: &lumen_html::Document, parent: NodeId) -> Result<Vec<NodeId>, Error> {
    let mut out = Vec::new();
    let mut child = document.first_child(parent)?;
    while let Some(id) = child {
        out.push(id);
        child = document.next_sibling(id)?;
    }
    Ok(out)
}

fn materialized_attribute_by_name(
    document: &lumen_html::Document,
    element: NodeId,
    qualified_name: &str,
) -> Option<NodeId> {
    let NodeKind::Element { attributes, .. } = document.kind(element).ok()? else {
        return None;
    };
    let index = attributes
        .iter()
        .position(|(name, _)| name.as_str() == qualified_name)?;
    document
        .materialized_attribute_nodes(element)?
        .iter()
        .find_map(|&(materialized_index, attribute)| {
            (materialized_index == index).then_some(attribute)
        })
}

fn materialized_attribute_by_ns(
    document: &lumen_html::Document,
    element: NodeId,
    namespace_uri: Option<&str>,
    local_name: &str,
) -> Option<NodeId> {
    let NodeKind::Element { attributes, .. } = document.kind(element).ok()? else {
        return None;
    };
    let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
    let materialized = document.materialized_attribute_nodes(element)?;
    attributes
        .iter()
        .enumerate()
        .find_map(|(index, (name, _))| {
            let attr_namespace = document.attribute_namespace_uri_at(element, index);
            let attr_local = if attr_namespace.is_some() {
                name.as_str()
                    .rsplit_once(':')
                    .map_or(name.as_str(), |(_, local)| local)
            } else {
                name.as_str()
            };
            (attr_namespace == namespace_uri && attr_local == local_name)
                .then(|| {
                    materialized
                        .iter()
                        .find_map(|&(materialized_index, attribute)| {
                            (materialized_index == index).then_some(attribute)
                        })
                })
                .flatten()
        })
}

fn element_scroll(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    element: NodeId,
    args: lumen_bind::OneOrNumberPair<scrolling::ScrollToOptions>,
    relative: bool,
) -> lumen::embed::Promise<()> {
    match scrolling::element_scroll_target(realm, element, scrolling::ElementScrollPurpose::Method)
    {
        Ok(Some(node)) => window_globals::scroll_node(ctx, realm.clone(), node, args, relative),
        Ok(None) => lumen::embed::Promise::ready::<OpError>(Ok(())),
        Err(error) => lumen::embed::Promise::rejected(error),
    }
}

#[lumen_bind::class(name = "Element", extends = DomNode, hint(js(webidl)))]
pub struct DomElement {
    base: DomNode,
}
#[lumen_bind::methods]
impl DomElement {
    #[getter]
    fn ontoggle(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.handler_value(ctx, &this.0, "toggle")
    }

    #[setter]
    fn set_ontoggle(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.base.set_handler(ctx, &this.0, "toggle", callback);
    }

    #[getter]
    fn onclick(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.handler_value(ctx, &this.0, "click")
    }

    #[setter]
    fn set_onclick(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.base.set_handler(ctx, &this.0, "click", callback);
    }

    #[method(name = "getElementsByTagName", coerce)]
    fn get_elements_by_tag_name(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        qualified_name: &str,
    ) -> Value {
        self.base.descendant_collection(
            ctx,
            this.0,
            format!("tag:{}:{qualified_name}", qualified_name.len()),
            DescendantFilter::Tag(qualified_name.to_owned()),
        )
    }

    #[method(name = "getElementsByTagNameNS", coerce)]
    fn get_elements_by_tag_name_ns(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Value {
        let namespace_uri = namespace_uri
            .filter(|namespace_uri| !namespace_uri.is_empty())
            .map(str::to_owned);
        let key = tag_ns_collection_key(namespace_uri.as_deref(), local_name);
        self.base.descendant_collection(
            ctx,
            this.0,
            key,
            DescendantFilter::TagNs(namespace_uri, local_name.to_owned()),
        )
    }

    #[method(name = "getElementsByClassName", coerce)]
    fn get_elements_by_class_name(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        class_names: &str,
    ) -> Value {
        let key = format!("class:{}:{class_names}", class_names.len());
        let names = html_space_tokens(class_names).map(str::to_owned).collect();
        self.base
            .descendant_collection(ctx, this.0, key, DescendantFilter::Class(names))
    }

    #[method(name = "getAttributeNames")]
    fn get_attribute_names(&self) -> OpResult<Vec<String>> {
        let session = self.base.realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(self.base.id).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "attributes require an element"));
        };
        Ok(attributes
            .iter()
            .map(|(name, _)| name.as_str().to_owned())
            .collect())
    }

    #[method(coerce)]
    fn toggle_attribute(&self, ctx: &mut Ctx, name: &str, force: Option<bool>) -> OpResult<bool> {
        let name = self.base.normalized_attribute_name(name);
        if !lumen_html::xml::is_valid_attribute_local_name(name.as_ref()) {
            return Err(error_reporting::dom_exception(
                ctx,
                "InvalidCharacterError",
                "attribute name is not a valid attribute local name",
            ));
        }
        let present = self.base.get_attribute(name.as_ref())?.is_some();
        let add = force.unwrap_or(!present);
        if add == present {
            return Ok(present);
        }
        if add {
            self.base.set_attribute(ctx, name.as_ref(), "")?;
            Ok(true)
        } else {
            self.base.remove_attribute(ctx, name.as_ref())?;
            Ok(false)
        }
    }

    #[getter(name = "attributeStyleMap")]
    fn attribute_style_map(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .base
            .collections
            .borrow()
            .get("attributeStyleMap")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = cssom::attribute_style_map(ctx, self.base.realm.clone(), self.base.id, this.0);
        self.base.collections.borrow_mut().insert(
            "attributeStyleMap".into(),
            ctx.weak_value(&value).expect("style map object"),
        );
        value
    }

    #[method(name = "computedStyleMap")]
    fn computed_style_map(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .base
            .collections
            .borrow()
            .get("computedStyleMap")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = cssom::computed_style_map(ctx, self.base.realm.clone(), self.base.id, this.0);
        self.base.collections.borrow_mut().insert(
            "computedStyleMap".into(),
            ctx.weak_value(&value).expect("computed style map object"),
        );
        value
    }

    #[getter]
    fn attributes(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        attributes::named_map(ctx, &self.base, this.0)
    }

    #[method(name = "getAttributeNode", coerce)]
    fn get_attribute_node(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        attributes::get_attribute_node(ctx, &self.base, name)
    }

    #[method(name = "getAttributeNodeNS", coerce)]
    fn get_attribute_node_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<Value> {
        attributes::get_attribute_node_ns(ctx, &self.base, namespace_uri, local_name)
    }

    #[method(name = "setAttributeNode")]
    fn set_attribute_node(
        &self,
        ctx: &mut Ctx,
        attribute: &attributes::DomAttr,
    ) -> OpResult<Value> {
        attributes::set_attribute_node(ctx, &self.base, attribute, false)
    }

    #[method(name = "setAttributeNodeNS")]
    fn set_attribute_node_ns(
        &self,
        ctx: &mut Ctx,
        attribute: &attributes::DomAttr,
    ) -> OpResult<Value> {
        attributes::set_attribute_node(ctx, &self.base, attribute, true)
    }

    #[method(name = "removeAttributeNode")]
    fn remove_attribute_node(
        &self,
        ctx: &mut Ctx,
        attribute: &attributes::DomAttr,
    ) -> OpResult<Value> {
        attributes::remove_attribute_node(ctx, &self.base, attribute)
    }

    fn request_fullscreen(&self, ctx: &mut Ctx) -> lumen::embed::Promise<()> {
        lumen::embed::Promise::ready(presentation::request_fullscreen(
            ctx,
            &self.base.realm,
            self.base.id,
        ))
    }

    fn request_pointer_lock(
        &self,
        ctx: &mut Ctx,
        options: Option<Value>,
    ) -> lumen::embed::Promise<()> {
        lumen::embed::Promise::ready(presentation::request_pointer_lock(
            ctx,
            &self.base.realm,
            self.base.id,
            options,
        ))
    }

    fn animate(&self, ctx: &mut Ctx, keyframes: Value, options: Option<Value>) -> OpResult<Value> {
        animations::animate(ctx, &self.base.realm, self.base.id, keyframes, options)
    }

    #[method(name = "getAnimations")]
    fn get_animations(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        let subtree = ui_events::dictionary_boolean(ctx, &options, "subtree", false)?;
        animations::for_element(ctx, &self.base.realm, self.base.id, subtree)
    }

    #[method(coerce)]
    fn scroll_into_view(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        options: Option<scrolling::IntoViewOptions>,
    ) -> OpResult<lumen::embed::Promise<()>> {
        let (realm, node) = ctx.with_instance::<Self, _>(&this.0, |element| {
            (element.base.realm.clone(), element.base.id)
        })?;
        Ok(scrolling::into_view(
            ctx,
            &realm,
            node,
            options.unwrap_or_default(),
        ))
    }

    fn get_bounding_client_rect(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.base.realm.flush_layout()?;
        Ok(geometry::bounding_client_rect_value(
            ctx,
            &self.base.realm,
            self.base.id,
        ))
    }

    fn get_client_rects(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.base.realm.flush_layout()?;
        Ok(geometry::client_rect_list_value(
            ctx,
            &self.base.realm,
            self.base.id,
        ))
    }

    #[getter]
    fn offset_left(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.offset_left)),
        )
    }

    #[getter]
    fn offset_top(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.offset_top)),
        )
    }

    #[getter]
    fn offset_width(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.offset_width)),
        )
    }

    #[getter]
    fn offset_height(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.offset_height)),
        )
    }

    #[getter]
    fn client_width(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.client_width)),
        )
    }

    #[getter]
    fn client_height(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.client_height)),
        )
    }

    #[getter]
    fn scroll_width(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.scroll_width)),
        )
    }

    #[getter]
    fn scroll_height(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.scroll_height)),
        )
    }

    #[getter]
    fn scroll_left(&self) -> OpResult<f64> {
        let Some(node) = scrolling::element_scroll_target(
            &self.base.realm,
            self.base.id,
            scrolling::ElementScrollPurpose::Getter,
        )?
        else {
            return Ok(0.0);
        };
        scrolling::position(&self.base.realm, node).map(|position| position.0)
    }

    #[getter]
    fn scroll_top(&self) -> OpResult<f64> {
        let Some(node) = scrolling::element_scroll_target(
            &self.base.realm,
            self.base.id,
            scrolling::ElementScrollPurpose::Getter,
        )?
        else {
            return Ok(0.0);
        };
        scrolling::position(&self.base.realm, node).map(|position| position.1)
    }

    #[getter]
    fn offset_parent(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.base.realm.flush_layout()?;
        let parent = geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
            .and_then(|geometry| geometry.offset_parent);
        Ok(self.base.realm.wrap_option(ctx, parent))
    }

    #[setter]
    fn set_scroll_left(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> {
        let Some(node) = scrolling::element_scroll_target(
            &self.base.realm,
            self.base.id,
            scrolling::ElementScrollPurpose::Setter,
        )?
        else {
            return Ok(());
        };
        let (_, top) = scrolling::position(&self.base.realm, node)?;
        scrolling::apply_offset(
            ctx,
            &self.base.realm,
            node,
            value,
            top,
            scrolling::ScrollBehavior::Auto,
        )
    }

    #[setter]
    fn set_scroll_top(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> {
        let Some(node) = scrolling::element_scroll_target(
            &self.base.realm,
            self.base.id,
            scrolling::ElementScrollPurpose::Setter,
        )?
        else {
            return Ok(());
        };
        let (left, _) = scrolling::position(&self.base.realm, node)?;
        scrolling::apply_offset(
            ctx,
            &self.base.realm,
            node,
            left,
            value,
            scrolling::ScrollBehavior::Auto,
        )
    }

    #[method(coerce)]
    fn scroll(
        &self,
        ctx: &mut Ctx,
        #[varargs] args: lumen_bind::OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> lumen::embed::Promise<()> {
        element_scroll(ctx, &self.base.realm, self.base.id, args, false)
    }

    #[method(coerce)]
    fn scroll_to(
        &self,
        ctx: &mut Ctx,
        #[varargs] args: lumen_bind::OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> lumen::embed::Promise<()> {
        element_scroll(ctx, &self.base.realm, self.base.id, args, false)
    }

    #[method(coerce)]
    fn scroll_by(
        &self,
        ctx: &mut Ctx,
        #[varargs] args: lumen_bind::OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> lumen::embed::Promise<()> {
        element_scroll(ctx, &self.base.realm, self.base.id, args, true)
    }

    fn attach_shadow(&self, ctx: &mut Ctx, options: Value) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let mode = match ctx
            .get_member(&options, "mode")
            .map_err(|_| OpError::new("TypeError", "shadow mode is required"))?
        {
            Value::Str(value) if value.as_str() == "open" => lumen_html::ShadowMode::Open,
            Value::Str(value) if value.as_str() == "closed" => lumen_html::ShadowMode::Closed,
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "shadow mode must be open or closed",
                ));
            }
        };
        let slot_assignment = match ctx
            .get_member(&options, "slotAssignment")
            .map_err(|_| OpError::new("TypeError", "slotAssignment getter failed"))?
        {
            Value::Undefined => lumen_html::SlotAssignmentMode::Named,
            Value::Str(value) if value.as_str() == "named" => lumen_html::SlotAssignmentMode::Named,
            Value::Str(value) if value.as_str() == "manual" => {
                lumen_html::SlotAssignmentMode::Manual
            }
            _ => {
                return Err(OpError::new(
                    "TypeError",
                    "slotAssignment must be named or manual",
                ));
            }
        };
        let boolean_option = |ctx: &mut Ctx, name: &str| -> OpResult<bool> {
            ctx.get_member(&options, name)
                .map(|value| matches!(value, Value::Bool(true)))
                .map_err(|_| OpError::new("TypeError", format!("{name} getter failed")))
        };
        let shadow_options = lumen_html::ShadowOptions {
            mode,
            slot_assignment,
            delegates_focus: boolean_option(ctx, "delegatesFocus")?,
            clonable: boolean_option(ctx, "clonable")?,
            serializable: boolean_option(ctx, "serializable")?,
            declarative: false,
        };
        let root = self
            .base
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attach_shadow_with_options(self.base.id, shadow_options)
            .map_err(|error| {
                if error == Error::Hierarchy {
                    OpError::new("NotSupportedError", "host already has a shadow root")
                } else {
                    dom_error(error)
                }
            })?;
        Ok(self.base.realm.wrap(ctx, root))
    }
    #[getter]
    fn shadow_root(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let root = {
            let session = self.base.realm.session.borrow();
            let document = session.document();
            document
                .shadow_root(self.base.id)
                .map_err(dom_error)?
                .filter(|root| {
                    document.shadow_mode(*root).ok().flatten() == Some(lumen_html::ShadowMode::Open)
                })
        };
        Ok(self.base.realm.wrap_option(ctx, root))
    }
    #[getter]
    fn slot(&self) -> OpResult<String> {
        Ok(self.base.get_null_attribute("slot")?.unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_slot(&self, value: &str) -> OpResult<()> {
        self.base.set_attribute_core("slot", value)
    }

    #[getter(name = "outerHTML")]
    fn outer_html(&self) -> OpResult<String> {
        self.base.outer_html()
    }

    #[setter(name = "outerHTML", coerce)]
    fn set_outer_html(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.base
            .replace_outer_html(ctx, value)
            .map_err(|error| dom_markup_error(ctx, error))
    }

    #[method(name = "insertAdjacentHTML", coerce)]
    fn insert_adjacent_html(&self, ctx: &mut Ctx, position: &str, value: &str) -> OpResult<()> {
        self.base
            .insert_adjacent_markup(ctx, position, value)
            .map_err(|error| dom_markup_error(ctx, error))
    }

    #[method(name = "insertAdjacentElement", coerce)]
    fn insert_adjacent_element(
        &self,
        ctx: &mut Ctx,
        position: &str,
        element: Value,
    ) -> OpResult<Value> {
        // Release the native projection before adoption can rehome its payload.
        ctx.with_instance::<DomElement, _>(&element, |_| ())?;
        let point = {
            let session = self.base.realm.session.borrow();
            adjacent_dom_insertion_point(session.document(), self.base.id, position)
        }
        .map_err(|error| dom_markup_error(ctx, error))?;
        let Some((parent, before)) = point else {
            return Ok(Value::Null);
        };
        let before = before.map_or(Value::Null, |node| self.base.realm.wrap(ctx, node));
        insert_dom_node(ctx, &self.base.realm, parent, element, before)
    }

    #[method(name = "insertAdjacentText", coerce)]
    fn insert_adjacent_text(&self, ctx: &mut Ctx, position: &str, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let point = {
            let session = self.base.realm.session.borrow();
            adjacent_dom_insertion_point(session.document(), self.base.id, position)
        }
        .map_err(|error| dom_markup_error(ctx, error))?;
        let Some((parent, before)) = point else {
            return Ok(());
        };
        let mut session = self.base.realm.session.borrow_mut();
        let document = session.document_mut();
        let text = document
            .create(NodeKind::Text(value.into()))
            .map_err(dom_error)?;
        if let Err(error) = document.insert_before(parent, text, before) {
            document.destroy_subtree(text).map_err(dom_error)?;
            return Err(dom_error(error));
        }
        drop(session);
        self.base.realm.invalidate_textarea_ancestor(parent);
        self.base.realm.flush_script_activations(ctx)
    }
}

#[lumen_bind::class(name = "HTMLElement", extends = DomElement, hint(js(webidl)))]
pub struct DomHtmlElement {
    base: DomElement,
}
#[lumen_bind::class(name = "DOMStringMap", hint(js(webidl)))]
pub struct DomDomStringMap {}
#[lumen_bind::methods]
impl DomDomStringMap {}

#[lumen_bind::class(name = "HTMLHtmlElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlHtmlElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlHtmlElement {}
#[lumen_bind::class(name = "HTMLHeadElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlHeadElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlHeadElement {}
#[lumen_bind::class(name = "HTMLDivElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlDivElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlDivElement {
    #[getter]
    fn align(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_attribute("align")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_align(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("align", value)
    }
}
#[lumen_bind::class(name = "HTMLBRElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlBrElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlBrElement {
    #[getter]
    fn clear(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_attribute("clear")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_clear(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("clear", value)
    }
}
#[lumen_bind::class(name = "HTMLBodyElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlBodyElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlBodyElement {
    #[getter]
    fn onload(&self, ctx: &mut Ctx) -> OpResult<Value> {
        event_content_handlers::window_handler_value(ctx, &self.base.base.base.realm, "load")
    }

    #[setter]
    fn set_onload(&self, ctx: &mut Ctx, callback: Option<lumen::embed::JsFunction>) {
        event_content_handlers::set_window_handler(
            ctx,
            &self.base.base.base.realm,
            "load",
            callback,
        );
    }

    #[getter]
    fn onerror(&self, ctx: &mut Ctx) -> OpResult<Value> {
        event_content_handlers::window_handler_value(ctx, &self.base.base.base.realm, "error")
    }

    #[setter]
    fn set_onerror(&self, ctx: &mut Ctx, callback: Option<lumen::embed::JsFunction>) {
        event_content_handlers::set_window_handler(
            ctx,
            &self.base.base.base.realm,
            "error",
            callback,
        );
    }
}
#[lumen_bind::class(name = "HTMLTitleElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlTitleElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlTitleElement {}
#[lumen_bind::class(name = "HTMLBaseElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlBaseElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlBaseElement {
    #[getter]
    fn href(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        let href = {
            let session = node.realm.session.borrow();
            html_base_href(session.document(), node.id, node.realm.is_html_document)
        };
        let href = href.unwrap_or_default();
        let fallback = node.realm.fallback_base_url();
        Ok(lumen_common::url::parse(&href, Some(&fallback))
            .map(|url| url.href())
            .unwrap_or(href))
    }

    #[setter(coerce)]
    fn set_href(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("href", value)
    }

    #[getter]
    fn target(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("target")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_target(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("target", value)
    }
}
#[lumen_bind::class(name = "HTMLLinkElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlLinkElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlLinkElement {
    #[getter]
    fn href(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        let Some(href) = node.get_null_attribute("href")? else {
            return Ok(String::new());
        };
        let base = node.realm.base_url();
        Ok(lumen_common::url::parse(&href, Some(&base))
            .map(|url| url.href())
            .unwrap_or(href))
    }

    #[setter(coerce)]
    fn set_href(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("href", value)
    }
}
#[lumen_bind::class(name = "HTMLScriptElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlScriptElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomHtmlScriptElement {
    #[method(coerce)]
    fn supports(ctx: &mut Ctx, script_type: &str) -> bool {
        match script_type {
            "classic" => true,
            "module" => realm_services::RealmServices::<ScriptCapabilities>::current(ctx)
                .is_some_and(|capabilities| capabilities.modules.get()),
            // Import-map preparation is incomplete and speculation-rule execution
            // is absent. Recognizing their type attributes is not support.
            _ => false,
        }
    }

    #[getter(name = "async")]
    fn script_async(&self) -> OpResult<bool> {
        let node = &self.base.base.base;
        Ok(node.realm.scripts.borrow().force_async(node.id) || node.has_null_attribute("async")?)
    }

    #[setter(name = "async", coerce)]
    fn set_async(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        node.realm.scripts.borrow_mut().clear_force_async(node.id);
        if value {
            node.set_attribute_core("async", "")
        } else {
            node.remove_attribute_core("async")
        }
    }

    #[getter(name = "defer")]
    fn script_defer(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("defer")
    }

    #[setter(name = "defer", coerce)]
    fn set_defer(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if value {
            node.set_attribute_core("defer", "")
        } else {
            node.remove_attribute_core("defer")
        }
    }

    #[getter(name = "noModule")]
    fn no_module(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("nomodule")
    }

    #[setter(name = "noModule", coerce)]
    fn set_no_module(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if value {
            node.set_attribute_core("nomodule", "")
        } else {
            node.remove_attribute_core("nomodule")
        }
    }

    #[getter(name = "type")]
    fn script_type(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("type")?
            .unwrap_or_default())
    }

    #[setter(name = "type", coerce)]
    fn set_type(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("type", value)?;
        self.base.base.base.realm.flush_script_activations(ctx)
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        let Some(source) = node.get_null_attribute("src")? else {
            return Ok(String::new());
        };
        let base = node.realm.base_url();
        Ok(lumen_common::url::parse(&source, Some(&base))
            .map(|url| url.href())
            .unwrap_or(source))
    }

    #[setter(coerce)]
    fn set_src(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("src", value)?;
        self.base.base.base.realm.flush_script_activations(ctx)
    }

    #[getter]
    fn text(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        Ok(script_loading::script_child_text(
            node.realm.session.borrow().document(),
            node.id,
        ))
    }

    #[setter(coerce)]
    fn set_text(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.base.base.base.set_text_content(ctx, value)
    }
}

#[lumen_bind::class(name = "HTMLImageElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomHtmlImageElement {
    base: DomHtmlElement,
}

impl DomHtmlImageElement {
    fn image_node(&self) -> &DomNode {
        &self.base.base.base
    }

    fn image_snapshot(&self) -> image_loading::ImageSnapshot {
        let node = self.image_node();
        let base = node.realm.base_url();
        let snapshot = {
            let session = node.realm.session.borrow();
            node.realm
                .images
                .snapshot(session.document(), node.id, &base)
        };
        let _ = node.realm.sync_image_bitmaps();
        snapshot
    }
}

#[lumen_bind::methods]
impl DomHtmlImageElement {
    /// The legacy `Image(width, height)` factory uses the same native class as
    /// queried and `createElement("img")` elements, preserving node identity.
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        #[default(0)] width: u32,
        #[default(0)] height: u32,
    ) -> OpResult<NodeConstructorResult<Self>> {
        let global = ctx.global_object();
        let document_value = ctx
            .get_member(&global, "document")
            .map_err(|_| OpError::new("TypeError", "Image has no active document"))?;
        let realm = ctx
            .with_instance::<DomDocument, _>(&document_value, |document| document.realm.clone())
            .map_err(|_| OpError::new("TypeError", "Image has no active document"))?;
        let id = realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "img".into(),
                attributes: Vec::new(),
            })
            .map_err(dom_error)?;
        if width != 0 {
            realm
                .session
                .borrow_mut()
                .document_mut()
                .set_attribute(id, "width", &width.to_string())
                .map_err(dom_error)?;
        }
        if height != 0 {
            realm
                .session
                .borrow_mut()
                .document_mut()
                .set_attribute(id, "height", &height.to_string())
                .map_err(dom_error)?;
        }
        let node = DomNode {
            base: DomEventTarget::node(&realm, id),
            realm: realm.clone(),
            id,
            collections: RefCell::new(HashMap::new()),
        };
        Ok(NodeConstructorResult::new(node, |base| Self {
            base: DomHtmlElement {
                base: DomElement { base },
            },
        }))
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let node = self.image_node();
        let Some(source) = node.get_null_attribute("src")? else {
            return Ok(String::new());
        };
        let base = node.realm.base_url();
        let resolved = image_loading::resolved_url(&source, &base);
        Ok(if resolved.is_empty() {
            source
        } else {
            resolved
        })
    }

    #[setter(coerce)]
    fn set_src(&self, source: &str) -> OpResult<()> {
        self.image_node().set_attribute_core("src", source)
    }

    #[getter(rename(js = "crossOrigin"))]
    fn cross_origin(&self) -> OpResult<Option<String>> {
        let value = self.image_node().get_null_attribute("crossorigin")?;
        Ok(value.map(|value| {
            if value.eq_ignore_ascii_case("use-credentials") {
                "use-credentials".to_owned()
            } else {
                "anonymous".to_owned()
            }
        }))
    }

    #[setter(rename(js = "crossOrigin"), coerce)]
    fn set_cross_origin(&self, value: &str) -> OpResult<()> {
        self.image_node().set_attribute_core("crossorigin", value)
    }

    #[getter(rename(js = "currentSrc"))]
    fn current_src(&self) -> String {
        self.image_snapshot().current_src
    }

    #[getter]
    fn complete(&self) -> bool {
        self.image_snapshot().complete
    }

    #[getter(rename(js = "naturalWidth"))]
    fn natural_width(&self) -> u32 {
        self.image_snapshot().natural_width
    }

    #[getter(rename(js = "naturalHeight"))]
    fn natural_height(&self) -> u32 {
        self.image_snapshot().natural_height
    }

    #[getter]
    fn width(&self) -> OpResult<u32> {
        let node = self.image_node();
        match node.get_null_attribute("width")? {
            Some(value) => Ok(lumen_html::layout::canvas_dimension(Some(&value), 0)),
            None => Ok(self.image_snapshot().natural_width),
        }
    }

    #[setter]
    fn set_width(&self, width: u32) -> OpResult<()> {
        self.image_node()
            .set_attribute_core("width", &width.to_string())
    }

    #[getter]
    fn height(&self) -> OpResult<u32> {
        let node = self.image_node();
        match node.get_null_attribute("height")? {
            Some(value) => Ok(lumen_html::layout::canvas_dimension(Some(&value), 0)),
            None => Ok(self.image_snapshot().natural_height),
        }
    }

    #[setter]
    fn set_height(&self, height: u32) -> OpResult<()> {
        self.image_node()
            .set_attribute_core("height", &height.to_string())
    }

    #[getter]
    fn onload(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.image_node().base.handler_value(ctx, &this.0, "load")
    }

    #[setter]
    fn set_onload(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.image_node()
            .base
            .set_handler(ctx, &this.0, "load", callback);
    }

    #[getter]
    fn onerror(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.image_node().base.handler_value(ctx, &this.0, "error")
    }

    #[setter]
    fn set_onerror(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.image_node()
            .base
            .set_handler(ctx, &this.0, "error", callback);
    }
}
#[lumen_bind::class(name = "HTMLIFrameElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomIFrameElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomIFrameElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "iframe")
    }

    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        let Some(source) = node.get_null_attribute("src")? else {
            return Ok(String::new());
        };
        let base = node.realm.base_url();
        Ok(lumen_common::url::parse(&source, Some(&base))
            .map(|url| url.href())
            .unwrap_or(source))
    }

    #[setter(coerce)]
    fn set_src(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("src", value)
    }

    #[getter]
    fn srcdoc(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("srcdoc")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_srcdoc(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("srcdoc", value)
    }

    #[getter(name = "contentWindow")]
    fn content_window(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let frame = browsing_context::ensure_frame_for_node(
            ctx,
            &self.base.base.base.realm,
            self.base.base.base.id,
        )?;
        Ok(frame
            .and_then(|frame| frame.window_proxy())
            .unwrap_or(Value::Null))
    }

    #[getter(name = "contentDocument")]
    fn content_document(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let frame = browsing_context::ensure_frame_for_node(
            ctx,
            &self.base.base.base.realm,
            self.base.base.base.id,
        )?;
        let Some(frame) = frame else {
            return Ok(Value::Null);
        };
        let Some(caller_origin) = browsing_context::invocation_origin(ctx) else {
            return Ok(Value::Null);
        };
        if !caller_origin.same_origin(&frame.origin()) {
            return Ok(Value::Null);
        }
        let Some(document_realm) = frame.current_document() else {
            return Ok(Value::Null);
        };
        let handle = frame.realm_handle();
        ctx.with_host_realm(&handle, |ctx| document_realm.document_value(ctx))
            .map_err(browsing_context::host_realm_error)
    }
}

#[lumen_bind::class(name = "HTMLStyleElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomStyleElement {
    base: DomHtmlElement,
}

#[lumen_bind::methods]
impl DomStyleElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "style")
    }
    #[getter]
    fn sheet(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        cssom::style_element_sheet(
            ctx,
            &self.base.base.base.realm,
            self.base.base.base.id,
            this.0,
        )
    }
}

#[lumen_bind::class(
    name = "HTMLFormElement",
    extends = DomHtmlElement,
    hint(js(webidl, named_properties, override_builtins))
)]
pub struct DomFormElement {
    base: DomHtmlElement,
}

#[lumen_bind::methods]
impl DomFormElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "form")
    }

    #[getter(name = "acceptCharset")]
    fn accept_charset(&self) -> OpResult<String> {
        Ok(self.base.base.base.get_null_attribute("accept-charset")?.unwrap_or_default())
    }

    #[setter(name = "acceptCharset", coerce)]
    fn set_accept_charset(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("accept-charset", value)
    }

    #[getter]
    fn action(&self) -> OpResult<String> {
        html_interfaces::action_attribute_value(&self.base.base.base, "action")
    }

    #[setter(coerce)]
    fn set_action(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("action", value)
    }

    #[getter]
    fn method(&self) -> OpResult<String> {
        let value = self.base.base.base.get_null_attribute("method")?.unwrap_or_default();
        Ok(lumen_html::forms::normalized_form_method(&value).into())
    }

    #[setter(coerce)]
    fn set_method(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("method", value)
    }

    #[getter]
    fn enctype(&self) -> OpResult<String> {
        let value = self.base.base.base.get_null_attribute("enctype")?.unwrap_or_default();
        Ok(lumen_html::forms::normalized_form_enctype(&value).into())
    }

    #[setter(coerce)]
    fn set_enctype(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("enctype", value)
    }

    #[getter]
    fn encoding(&self) -> OpResult<String> {
        self.enctype()
    }

    #[setter(coerce)]
    fn set_encoding(&self, value: &str) -> OpResult<()> {
        self.set_enctype(value)
    }

    #[getter]
    fn target(&self) -> OpResult<String> {
        Ok(self.base.base.base.get_null_attribute("target")?.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_target(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("target", value)
    }

    #[getter(name = "noValidate")]
    fn no_validate(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("novalidate")
    }

    #[setter(name = "noValidate", coerce)]
    fn set_no_validate(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if value {
            node.set_attribute_core("novalidate", "")
        } else {
            node.remove_attribute_core("novalidate")
        }
    }

    #[getter]
    fn autocomplete(&self) -> OpResult<String> {
        let value = self.base.base.base.get_null_attribute("autocomplete")?.unwrap_or_default();
        Ok(if value.eq_ignore_ascii_case("off") { "off" } else { "on" }.into())
    }

    #[setter(coerce)]
    fn set_autocomplete(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("autocomplete", value)
    }

    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let node = &self.base.base.base;
        let (realm, form) = node.realm.resolve_adopted_node(node.id);
        let session = realm.session.borrow();
        lumen_html::forms::form_element_count(session.document(), form).map_err(dom_error)
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let node = &self.base.base.base;
        let (realm, form) = node.realm.resolve_adopted_node(node.id);
        let session = realm.session.borrow();
        let item = lumen_html::forms::form_element_at(session.document(), form, index)
            .map_err(dom_error)?;
        Ok(item.map_or(Value::Undefined, |item| realm.wrap(ctx, item)))
    }

    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> OpResult<bool> {
        let node = &self.base.base.base;
        let (realm, form) = node.realm.resolve_adopted_node(node.id);
        collections::form_named_property_supported(&realm, form, name)
    }

    #[method(hint(js(named_names)))]
    fn named_names(&self) -> OpResult<Vec<String>> {
        let node = &self.base.base.base;
        let (realm, form) = node.realm.resolve_adopted_node(node.id);
        collections::form_named_property_names(&realm, form)
    }

    #[method(hint(js(named_getter)))]
    fn named_getter(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        name: &str,
    ) -> OpResult<Value> {
        let node = &self.base.base.base;
        let (realm, form) = node.realm.resolve_adopted_node(node.id);
        collections::form_named_property_getter(ctx, &realm, form, this.0, name)
    }

    #[getter]
    fn elements(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let node = &self.base.base.base;
        if let Some(value) = node
            .collections
            .borrow()
            .get("elements")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomHtmlFormControlsCollection {
            base: DomHtmlCollection {
                base: DomNodeList::form_elements(node.realm.clone(), node.id, this.0.clone()),
            },
        });
        node.collections.borrow_mut().insert(
            "elements".into(),
            ctx.weak_value(&value).expect("form elements collection"),
        );
        value
    }
    #[getter]
    fn onsubmit(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .base
            .handler_value(ctx, &this.0, "submit")
    }

    #[setter]
    fn set_onsubmit(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .base
            .set_handler(ctx, &this.0, "submit", callback);
    }

    #[getter]
    fn onreset(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .base
            .handler_value(ctx, &this.0, "reset")
    }

    #[setter]
    fn set_onreset(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .base
            .set_handler(ctx, &this.0, "reset", callback);
    }

    fn submit(&self, ctx: &mut Ctx) -> OpResult<()> {
        let node = &self.base.base.base;
        node.realm.submit_form_legacy(ctx, node.id)?;
        Ok(())
    }

    fn request_submit(&self, ctx: &mut Ctx, submitter: Option<&DomNode>) -> OpResult<()> {
        let node = &self.base.base.base;
        let submitter = if let Some(submitter) = submitter {
            if !Rc::ptr_eq(&node.realm, &submitter.realm) {
                return Err(OpError::new(
                    "NotFoundError",
                    "Submitter belongs to another document",
                ));
            }
            let session = node.realm.session.borrow();
            let document = session.document();
            if !lumen_html::forms::is_submit_button(document, submitter.id) {
                return Err(OpError::new(
                    "TypeError",
                    "Submitter must be a submit button",
                ));
            }
            if lumen_html::forms::form_owner(document, submitter.id) != Some(node.id) {
                return Err(OpError::new(
                    "NotFoundError",
                    "Submitter does not belong to this form",
                ));
            }
            drop(session);
            Some(submitter.id)
        } else {
            None
        };
        node.realm.submit_form(ctx, node.id, submitter)?;
        Ok(())
    }

    fn reset(&self, ctx: &mut Ctx) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::reset_form(ctx, &node.realm, node.id, &node.realm.forms)?;
        Ok(())
    }

    fn check_validity(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let node = &self.base.base.base;
        forms::check_form_validity(ctx, &node.realm, node.id, &node.realm.forms)
    }

    fn report_validity(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let node = &self.base.base.base;
        forms::report_form_validity(ctx, &node.realm, node.id, &node.realm.forms)
    }
}

#[lumen_bind::class(name = "HTMLDetailsElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomDetailsElement {
    base: DomHtmlElement,
}

#[lumen_bind::methods]
impl DomDetailsElement {
    #[getter]
    fn open(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("open")
    }

    #[setter(coerce)]
    fn set_open(&self, open: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if open {
            node.set_attribute_core("open", "")
        } else {
            node.remove_attribute_core("open")
        }
    }
}

#[lumen_bind::class(name = "HTMLInputElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomInputElement {
    base: DomHtmlElement,
}

/// Web IDL's `[LegacyNullToEmptyString]` conversion for form-control values.
struct LegacyNullToEmptyString(String);

impl<'a> lumen_bind::FromArg<'a, lumen::embed::JsHost> for LegacyNullToEmptyString {
    fn from_arg(
        cx: &'a <lumen::embed::JsHost as lumen_bind::Host>::Cx<'_>,
        value: &'a Value,
        at: lumen_bind::Slot,
    ) -> Result<Self, Value> {
        if matches!(value, Value::Null) {
            Ok(Self(String::new()))
        } else {
            <String as lumen_bind::FromArg<'a, lumen::embed::JsHost>>::from_arg(cx, value, at)
                .map(Self)
        }
    }
}

#[lumen_bind::class(name = "HTMLSelectElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomSelectElement {
    base: DomHtmlElement,
}

#[lumen_bind::class(
    name = "HTMLOptionsCollection",
    extends = DomHtmlCollection,
    hint(js(webidl, named_properties))
)]
pub struct DomHtmlOptionsCollection {
    base: DomHtmlCollection,
    realm: Rc<DomRealm>,
    select: NodeId,
}

#[lumen_bind::methods]
impl DomHtmlOptionsCollection {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> OpResult<bool> {
        if name.is_empty() {
            return Ok(false);
        }
        let (realm, select) = self.realm.resolve_adopted_node(self.select);
        let session = realm.session.borrow();
        lumen_html::forms::select_option_named_item(session.document(), select, name)
            .map(|option| option.is_some())
            .map_err(dom_error)
    }

    #[method(hint(js(named_names)))]
    fn named_names(&self) -> OpResult<Vec<String>> {
        let (realm, select) = self.realm.resolve_adopted_node(self.select);
        let session = realm.session.borrow();
        let document = session.document();
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut error = None;
        lumen_html::forms::for_each_select_option(document, select, |option, _| {
            for attribute in ["id", "name"] {
                match document.get_attribute_ns_ref(option, None, attribute) {
                    Ok(Some(value)) if !value.is_empty() && seen.insert(value.to_owned()) => {
                        names.push(value.to_owned());
                    }
                    Ok(_) => {}
                    Err(value) => {
                        error = Some(value);
                        return false;
                    }
                }
            }
            true
        })
        .map_err(dom_error)?;
        if let Some(error) = error {
            return Err(dom_error(error));
        }
        Ok(names)
    }

    #[method(hint(js(named_getter)))]
    fn named_getter(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        forms::select_option_named_item(ctx, &self.realm, self.select, name)
    }

    #[method(coerce)]
    fn named_item(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        forms::select_option_named_item(ctx, &self.realm, self.select, name)
    }

    #[method(coerce)]
    fn add(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        element: forms::SelectAddElement,
        #[default(forms::SelectAddBefore::Append)] before: forms::SelectAddBefore,
    ) -> OpResult<()> {
        let (realm, select) = ctx.with_instance::<Self, _>(&this.0, |collection| {
            (collection.realm.clone(), collection.select)
        })?;
        forms::add_select_element(ctx, &realm, select, element, before)
    }

    // Lumen's indexed native-object hooks are registered on the concrete class,
    // so inheriting HTMLCollection's prototype does not install these traps for
    // HTMLOptionsCollection instances. Keep them backed by the shared form
    // algorithm so indexed access and selectedIndex see the same live options.
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let (realm, select) = self.realm.resolve_adopted_node(self.select);
        let session = realm.session.borrow();
        lumen_html::forms::select_option_count(session.document(), select).map_err(dom_error)
    }

    #[setter(coerce)]
    fn set_length(&self, length: u32) -> OpResult<()> {
        forms::set_select_options_length(&self.realm, self.select, length)
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let (realm, select) = self.realm.resolve_adopted_node(self.select);
        let option = {
            let session = realm.session.borrow();
            lumen_html::forms::select_option_at(session.document(), select, index)
                .map_err(dom_error)?
        };
        Ok(option.map_or(Value::Undefined, |id| realm.wrap(ctx, id)))
    }

    #[proto(setitem)]
    fn set_index(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        index: usize,
        option: Option<JsObject>,
    ) -> OpResult<()> {
        let (realm, select) = ctx.with_instance::<Self, _>(&this.0, |collection| {
            (collection.realm.clone(), collection.select)
        })?;
        forms::set_select_option_at(
            ctx,
            &realm,
            select,
            index,
            option.map(JsObject::into_value),
        )
    }

    #[getter]
    fn selected_index(&self) -> OpResult<i32> {
        Ok(forms::selected_index(&self.realm, self.select)? as i32)
    }

    #[setter]
    fn set_selected_index(&self, index: i32) -> OpResult<()> {
        forms::set_select_selected_index(
            &self.realm,
            &mut self.realm.forms.borrow_mut(),
            self.select,
            index as isize,
        )
    }
}

#[lumen_bind::methods]
impl DomSelectElement {
    #[getter]
    fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let node = &self.base.base.base;
        labels::control_labels(ctx, &node.realm, node.id, this.0, &node.collections)
    }

    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }

    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "select")
    }
    #[getter]
    fn options(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let node = &self.base.base.base;
        if let Some(value) = node
            .collections
            .borrow()
            .get("options")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let collection = ctx.new_instance(DomHtmlOptionsCollection {
            base: DomHtmlCollection {
                base: DomNodeList::select_options(node.realm.clone(), node.id, this.0),
            },
            realm: node.realm.clone(),
            select: node.id,
        });
        node.collections.borrow_mut().insert(
            "options".into(),
            ctx.weak_value(&collection).expect("options collection"),
        );
        collection
    }
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let node = &self.base.base.base;
        let (realm, select) = node.realm.resolve_adopted_node(node.id);
        let session = realm.session.borrow();
        lumen_html::forms::select_option_count(session.document(), select).map_err(dom_error)
    }
    #[setter(coerce)]
    fn set_length(&self, length: u32) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_select_options_length(&node.realm, node.id, length)
    }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let node = &self.base.base.base;
        let (realm, select) = node.realm.resolve_adopted_node(node.id);
        let option = {
            let session = realm.session.borrow();
            lumen_html::forms::select_option_at(session.document(), select, index)
                .map_err(dom_error)?
        };
        Ok(option.map_or(Value::Undefined, |option| realm.wrap(ctx, option)))
    }
    #[method(coerce)]
    fn item(&self, ctx: &mut Ctx, index: u32) -> OpResult<Value> {
        let node = &self.base.base.base;
        let (realm, select) = node.realm.resolve_adopted_node(node.id);
        let option = {
            let session = realm.session.borrow();
            lumen_html::forms::select_option_at(session.document(), select, index as usize)
                .map_err(dom_error)?
        };
        Ok(option.map_or(Value::Null, |option| realm.wrap(ctx, option)))
    }
    #[method(coerce)]
    fn named_item(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        let node = &self.base.base.base;
        forms::select_option_named_item(ctx, &node.realm, node.id, name)
    }
    #[getter]
    fn multiple(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("multiple")
    }
    #[setter(coerce)]
    fn set_multiple(&self, multiple: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if multiple {
            node.set_attribute_core("multiple", "")
        } else {
            node.remove_attribute_core("multiple")
        }
    }
    #[getter]
    fn required(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("required")
    }
    #[setter(coerce)]
    fn set_required(&self, required: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if required {
            node.set_attribute_core("required", "")
        } else {
            node.remove_attribute_core("required")
        }
    }
    #[getter]
    fn selected_options(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        let node = &self.base.base.base;
        if let Some(value) = node
            .collections
            .borrow()
            .get("selectedOptions")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let collection = ctx.new_instance(DomHtmlCollection {
            base: DomNodeList::selected_options(node.realm.clone(), node.id, this.0),
        });
        node.collections.borrow_mut().insert(
            "selectedOptions".into(),
            ctx.weak_value(&collection)
                .expect("selectedOptions collection"),
        );
        collection
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        forms::control_value(&self.base.base.base.realm, self.base.base.base.id)
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_control_value(
            &node.realm,
            &mut node.realm.forms.borrow_mut(),
            node.id,
            value,
        )
    }
    #[getter]
    fn form(&self, ctx: &mut Ctx) -> Value {
        let node = &self.base.base.base;
        forms::form_owner_value(ctx, &node.realm, node.id)
    }
    #[getter]
    fn selected_index(&self) -> OpResult<i32> {
        Ok(forms::selected_index(&self.base.base.base.realm, self.base.base.base.id)? as i32)
    }
    #[setter]
    fn set_selected_index(&self, index: i32) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_select_selected_index(
            &node.realm,
            &mut node.realm.forms.borrow_mut(),
            node.id,
            index as isize,
        )
    }

    #[method(coerce)]
    fn add(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        element: forms::SelectAddElement,
        #[default(forms::SelectAddBefore::Append)] before: forms::SelectAddBefore,
    ) -> OpResult<()> {
        let (realm, select) = ctx.with_instance::<Self, _>(&this.0, |select| {
            let node = &select.base.base.base;
            (node.realm.clone(), node.id)
        })?;
        forms::add_select_element(ctx, &realm, select, element, before)
    }
}

#[lumen_bind::class(name = "HTMLOptionElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomOptionElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomOptionElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "option")
    }
    #[getter]
    fn text(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        lumen_html::forms::option_text(node.realm.session.borrow().document(), node.id)
            .map_err(dom_error)
    }
    #[setter(coerce)]
    fn set_text(ctx: &mut Ctx, this: lumen_bind::This<Value>, text: &str) -> OpResult<()> {
        let (realm, id) = ctx.with_instance::<Self, _>(&this.0, |option| {
            let node = &option.base.base.base;
            (node.realm.clone(), node.id)
        })?;
        DomNode::replace_text_content(ctx, &realm, id, text)
    }
    #[getter]
    fn label(&self) -> OpResult<String> {
        match self.base.base.base.get_null_attribute("label")? {
            Some(label) => Ok(label),
            None => self.text(),
        }
    }
    #[setter(coerce)]
    fn set_label(&self, label: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("label", label)
    }
    #[getter]
    fn index(&self) -> OpResult<u32> {
        let node = &self.base.base.base;
        lumen_html::forms::option_index(node.realm.session.borrow().document(), node.id)
            .map(|index| index as u32)
            .map_err(dom_error)
    }
    #[getter]
    fn form(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        let form = {
            let session = node.realm.session.borrow();
            let document = session.document();
            lumen_html::forms::option_select(document, node.id)
                .map_err(dom_error)?
                .and_then(|select| lumen_html::forms::form_owner(document, select))
        };
        Ok(form.map_or(Value::Null, |form| node.realm.wrap(ctx, form)))
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        lumen_html::forms::option_value(
            self.base.base.base.realm.session.borrow().document(),
            self.base.base.base.id,
        )
        .ok_or_else(|| OpError::new("TypeError", "option value is unavailable"))
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("value", value)
    }
    #[getter]
    fn selected(&self) -> bool {
        forms::option_selected(&self.base.base.base.realm, self.base.base.base.id)
    }
    #[setter(coerce)]
    fn set_selected(&self, selected: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_option_selected(
            &node.realm,
            &mut node.realm.forms.borrow_mut(),
            node.id,
            selected,
        )
    }
    #[getter]
    fn default_selected(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("selected")
    }
    #[setter(coerce)]
    fn set_default_selected(&self, selected: bool) -> OpResult<()> {
        if selected {
            self.base.base.base.set_attribute_core("selected", "")
        } else {
            self.base.base.base.remove_attribute_core("selected")
        }
    }
}

#[lumen_bind::class(name = "HTMLTextAreaElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomTextAreaElement {
    base: DomHtmlElement,
}

fn textarea_length_attribute(node: &DomNode, name: &str) -> OpResult<i32> {
    let Some(value) = node.get_null_attribute(name)? else {
        return Ok(-1);
    };
    Ok(lumen_html::forms::parse_nonnegative_integer(&value)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or(-1))
}

fn set_textarea_length_attribute(
    ctx: &mut Ctx,
    node: &DomNode,
    name: &str,
    value: i32,
) -> OpResult<()> {
    if value < 0 {
        return Err(error_reporting::dom_exception(
            ctx,
            "IndexSizeError",
            "textarea length constraints cannot be negative",
        ));
    }
    node.set_attribute_core(name, &value.to_string())
}

fn textarea_size_attribute(node: &DomNode, name: &str, default: u32) -> OpResult<u32> {
    let value = node.get_null_attribute(name)?;
    Ok(value
        .and_then(|value| lumen_html::forms::parse_nonnegative_integer(&value))
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(default)
        .max(1))
}

#[lumen_bind::methods]
impl DomTextAreaElement {
    #[getter]
    fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let node = &self.base.base.base;
        labels::control_labels(ctx, &node.realm, node.id, this.0, &node.collections)
    }

    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }

    #[getter]
    fn dir_name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("dirname")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_dir_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("dirname", value)
    }

    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "textarea")
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        forms::control_value(&node.realm, node.id)
    }
    #[setter(coerce)]
    fn set_value(&self, value: LegacyNullToEmptyString) -> OpResult<()> {
        let node = &self.base.base.base;
        node.realm
            .set_control_value_and_selection(node.id, &value.0, true)
    }
    #[getter]
    fn required(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("required")
    }
    #[setter(coerce)]
    fn set_required(&self, required: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if required {
            node.set_attribute_core("required", "")
        } else {
            node.remove_attribute_core("required")
        }
    }
    #[getter(name = "type")]
    fn textarea_type(&self) -> &'static str {
        "textarea"
    }
    #[getter(name = "textLength")]
    fn text_length(&self) -> OpResult<u32> {
        let node = &self.base.base.base;
        let value = forms::control_value(&node.realm, node.id)?;
        Ok(u32::try_from(lumen_common::smuggle::utf16_unit_len(&value)).unwrap_or(u32::MAX))
    }
    #[getter(name = "maxLength")]
    fn max_length(&self) -> OpResult<i32> {
        textarea_length_attribute(&self.base.base.base, "maxlength")
    }
    #[setter(name = "maxLength", coerce)]
    fn set_max_length(&self, ctx: &mut Ctx, value: i32) -> OpResult<()> {
        set_textarea_length_attribute(ctx, &self.base.base.base, "maxlength", value)
    }
    #[getter(name = "minLength")]
    fn min_length(&self) -> OpResult<i32> {
        textarea_length_attribute(&self.base.base.base, "minlength")
    }
    #[setter(name = "minLength", coerce)]
    fn set_min_length(&self, ctx: &mut Ctx, value: i32) -> OpResult<()> {
        set_textarea_length_attribute(ctx, &self.base.base.base, "minlength", value)
    }
    #[getter]
    fn placeholder(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("placeholder")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_placeholder(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("placeholder", value)
    }
    #[getter(name = "readOnly")]
    fn read_only(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("readonly")
    }
    #[setter(name = "readOnly", coerce)]
    fn set_read_only(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if value {
            node.set_attribute_core("readonly", "")
        } else {
            node.remove_attribute_core("readonly")
        }
    }
    #[getter]
    fn rows(&self) -> OpResult<u32> {
        textarea_size_attribute(&self.base.base.base, "rows", 2)
    }
    #[setter(coerce)]
    fn set_rows(&self, value: u32) -> OpResult<()> {
        self.base.base.base.set_attribute_core("rows", &value.to_string())
    }
    #[getter]
    fn cols(&self) -> OpResult<u32> {
        textarea_size_attribute(&self.base.base.base, "cols", 20)
    }
    #[setter(coerce)]
    fn set_cols(&self, value: u32) -> OpResult<()> {
        self.base.base.base.set_attribute_core("cols", &value.to_string())
    }
    #[getter]
    fn wrap(&self) -> OpResult<&'static str> {
        let value = self.base.base.base.get_null_attribute("wrap")?;
        Ok(if value.is_some_and(|value| value.eq_ignore_ascii_case("hard")) {
            "hard"
        } else {
            "soft"
        })
    }
    #[setter(coerce)]
    fn set_wrap(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("wrap", value)
    }
    #[getter]
    fn default_value(&self) -> OpResult<String> {
        lumen_html::forms::default_value(
            self.base.base.base.realm.session.borrow().document(),
            self.base.base.base.id,
        )
        .ok_or_else(|| OpError::new("TypeError", "textarea defaultValue is unavailable"))
    }
    #[setter(coerce)]
    fn set_default_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_textarea_default_value(
            &node.realm,
            &mut node.realm.forms.borrow_mut(),
            node.id,
            value,
        )
    }
    #[getter]
    fn form(&self, ctx: &mut Ctx) -> Value {
        let node = &self.base.base.base;
        forms::form_owner_value(ctx, &node.realm, node.id)
    }
}
#[lumen_bind::methods]
impl DomInputElement {
    #[getter]
    fn read_only(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("readonly")
    }
    #[setter(coerce)]
    fn set_read_only(&self, read_only: bool) -> OpResult<()> {
        if read_only {
            self.base.base.base.set_attribute_core("readonly", "")
        } else {
            self.base.base.base.remove_attribute_core("readonly")
        }
    }

    #[getter]
    fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let node = &self.base.base.base;
        labels::control_labels(ctx, &node.realm, node.id, this.0, &node.collections)
    }

    #[getter]
    fn list(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        let datalist = {
            let session = node.realm.session.borrow();
            lumen_html::forms::input_list(session.document(), node.id).map_err(dom_error)?
        };
        Ok(node.realm.wrap_option(ctx, datalist))
    }

    #[getter]
    fn dir_name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("dirname")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_dir_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("dirname", value)
    }

    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "input")
    }
    #[getter(name = "type")]
    fn input_type(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("type")?
            .unwrap_or_else(|| "text".into()))
    }
    #[setter(name = "type", coerce)]
    fn set_input_type(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("type", value)
    }
    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }
    #[getter]
    fn required(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("required")
    }
    #[setter(coerce)]
    fn set_required(&self, required: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if required {
            node.set_attribute_core("required", "")
        } else {
            node.remove_attribute_core("required")
        }
    }
    #[getter]
    fn pattern(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("pattern")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_pattern(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("pattern", value)
    }
    #[getter]
    fn placeholder(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("placeholder")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_placeholder(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("placeholder", value)
    }
    #[getter(name = "defaultChecked")]
    fn default_checked(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("checked")
    }
    #[setter(name = "defaultChecked", coerce)]
    fn set_default_checked(&self, checked: bool) -> OpResult<()> {
        if checked {
            self.base.base.base.set_attribute_core("checked", "")
        } else {
            self.base.base.base.remove_attribute_core("checked")
        }
    }
    #[getter]
    fn default_value(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("value")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_default_value(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("value", value)
    }
    #[getter]
    fn min(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("min")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_min(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("min", value)
    }
    #[getter]
    fn max(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("max")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_max(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("max", value)
    }
    #[getter]
    fn step(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("step")?
            .unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_step(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("step", value)
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        forms::control_value(&self.base.base.base.realm, self.base.base.base.id)
    }
    #[setter(coerce)]
    fn set_value(&self, value: LegacyNullToEmptyString) -> OpResult<()> {
        self.base.set_value(&value.0)
    }
    #[getter]
    fn value_as_number(&self) -> OpResult<f64> {
        let node = &self.base.base.base;
        forms::input_value_as_number(&node.realm, node.id)
    }
    #[setter(coerce)]
    fn set_value_as_number(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::set_input_value_as_number(ctx, &node.realm, node.id, value)
    }
    #[getter]
    fn value_as_date(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        let value = forms::control_value(&node.realm, node.id)?;
        let milliseconds = {
            let session = node.realm.session.borrow();
            let document = session.document();
            if lumen_html::forms::input_date_type_supported(document, node.id) {
                lumen_html::forms::input_value_as_date_ms(document, node.id, &value)
            } else {
                None
            }
        };
        Ok(milliseconds.map_or(Value::Null, |milliseconds| {
            ctx.new_date_value(milliseconds)
        }))
    }
    #[setter(coerce)]
    fn set_value_as_date(&self, ctx: &mut Ctx, value: Option<JsObject>) -> OpResult<()> {
        let node = &self.base.base.base;
        let supported = {
            let session = node.realm.session.borrow();
            lumen_html::forms::input_date_type_supported(session.document(), node.id)
        };
        if !supported {
            return Err(error_reporting::dom_exception(
                ctx,
                "InvalidStateError",
                "valueAsDate does not apply to this input type",
            ));
        }
        let Some(date) = value else {
            return forms::set_control_value(
                &node.realm,
                &mut node.realm.forms.borrow_mut(),
                node.id,
                "",
            );
        };
        let date = date.into_value();
        let Some(milliseconds) = ctx.date_value(&date) else {
            return Err(OpError::type_error("valueAsDate requires a Date object"));
        };
        let converted = {
            let session = node.realm.session.borrow();
            lumen_html::forms::input_date_ms_value_string(session.document(), node.id, milliseconds)
        };
        let value = match converted {
            Ok(value) => value,
            Err(lumen_html::forms::InputNumberError::Unrepresentable) => String::new(),
            Err(lumen_html::forms::InputNumberError::UnsupportedType) => {
                return Err(error_reporting::dom_exception(
                    ctx,
                    "InvalidStateError",
                    "valueAsDate does not apply to this input type",
                ));
            }
            Err(lumen_html::forms::InputNumberError::StepAny) => {
                return Err(error_reporting::dom_exception(
                    ctx,
                    "InvalidStateError",
                    "valueAsDate does not apply to this input type",
                ));
            }
        };
        forms::set_control_value(
            &node.realm,
            &mut node.realm.forms.borrow_mut(),
            node.id,
            &value,
        )
    }
    #[method(coerce)]
    fn step_up(&self, ctx: &mut Ctx, #[default(1)] count: i32) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::step_input_value(
            ctx,
            &node.realm,
            node.id,
            count as i64,
            lumen_html::forms::InputStepDirection::Up,
        )
    }
    #[method(coerce)]
    fn step_down(&self, ctx: &mut Ctx, #[default(1)] count: i32) -> OpResult<()> {
        let node = &self.base.base.base;
        forms::step_input_value(
            ctx,
            &node.realm,
            node.id,
            count as i64,
            lumen_html::forms::InputStepDirection::Down,
        )
    }
    #[getter]
    fn files(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        forms::files_value(ctx, &node.realm, node.id, &node.realm.forms)
    }
    #[getter]
    fn form(&self, ctx: &mut Ctx) -> Value {
        let node = &self.base.base.base;
        forms::form_owner_value(ctx, &node.realm, node.id)
    }
    #[getter(name = "formAction")]
    fn form_action(&self) -> OpResult<String> {
        html_interfaces::form_action_value(&self.base.base.base)
    }
    #[setter(name = "formAction", coerce)]
    fn set_form_action(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("formaction", value)
    }
    #[getter(name = "formEnctype")]
    fn form_enctype(&self) -> OpResult<String> {
        html_interfaces::form_enctype_value(&self.base.base.base)
    }
    #[setter(name = "formEnctype", coerce)]
    fn set_form_enctype(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("formenctype", value)
    }
    #[getter(name = "formMethod")]
    fn form_method(&self) -> OpResult<String> {
        html_interfaces::form_method_value(&self.base.base.base)
    }
    #[setter(name = "formMethod", coerce)]
    fn set_form_method(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("formmethod", value)
    }
    #[getter(name = "formNoValidate")]
    fn form_no_validate(&self) -> OpResult<bool> {
        self.base.base.base.has_null_attribute("formnovalidate")
    }
    #[setter(name = "formNoValidate", coerce)]
    fn set_form_no_validate(&self, value: bool) -> OpResult<()> {
        let node = &self.base.base.base;
        if value {
            node.set_attribute_core("formnovalidate", "")
        } else {
            node.remove_attribute_core("formnovalidate")
        }
    }
    #[getter(name = "formTarget")]
    fn form_target(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("formtarget")?
            .unwrap_or_default())
    }
    #[setter(name = "formTarget", coerce)]
    fn set_form_target(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("formtarget", value)
    }
    fn show_picker(&self, ctx: &mut Ctx) -> OpResult<()> {
        let node = &self.base.base.base;
        node.realm.request_file_picker(ctx, node.id)
    }
}
#[lumen_bind::class(name = "HTMLTemplateElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomTemplateElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomTemplateElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "template")
    }
    #[getter]
    fn content(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        let content = node
            .realm
            .session
            .borrow()
            .document()
            .template_content(node.id)
            .map_err(dom_error)?
            .unwrap();
        Ok(node.realm.wrap(ctx, content))
    }
}
#[lumen_bind::methods]
impl DomHtmlElement {
    #[getter]
    fn dir(&self) -> String {
        let node = &self.base.base;
        let (realm, id) = node.realm.resolve_adopted_node(node.id);
        let session = realm.session.borrow();
        lumen_html::directionality::dir_attribute_state(session.document(), id)
            .map_or("", lumen_html::directionality::DirAttributeState::keyword)
            .to_owned()
    }

    #[setter(coerce)]
    fn set_dir(&self, value: &str) -> OpResult<()> {
        self.base.base.set_attribute_core("dir", value)
    }

    #[getter]
    fn popover(&self) -> OpResult<String> {
        let node = &self.base.base;
        dialog_popover::popover_value(&node.realm, node.id)
    }

    #[setter(coerce)]
    fn set_popover(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base;
        dialog_popover::set_popover_value(&node.realm, node.id, value)
    }

    #[method(name = "showPopover")]
    fn show_popover(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomHtmlElement, _>(&this.0, |element| {
            let node = &element.base.base;
            node.realm.resolve_adopted_node(node.id)
        })?;
        dialog_popover::show_popover(ctx, &realm, node)
    }

    #[method(name = "hidePopover")]
    fn hide_popover(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomHtmlElement, _>(&this.0, |element| {
            let node = &element.base.base;
            node.realm.resolve_adopted_node(node.id)
        })?;
        dialog_popover::hide_popover(ctx, &realm, node)
    }

    #[method(name = "togglePopover")]
    fn toggle_popover(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        force: Option<bool>,
    ) -> OpResult<bool> {
        let (realm, node) = ctx.with_instance::<DomHtmlElement, _>(&this.0, |element| {
            let node = &element.base.base;
            node.realm.resolve_adopted_node(node.id)
        })?;
        dialog_popover::toggle_popover(ctx, &realm, node, force)
    }

    fn click(&self, ctx: &mut Ctx) -> OpResult<()> {
        let node = &self.base.base;
        forms::click_element(ctx, &node.realm, &node.base, node.id)
    }

    #[getter]
    fn onvolumechange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        event_content_handlers::element_handler_value(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "volumechange",
        )
    }

    #[setter]
    fn set_onvolumechange(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        event_content_handlers::set_element_handler(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "volumechange",
            callback,
        );
    }

    #[getter]
    fn onload(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        event_content_handlers::element_handler_value(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "load",
        )
    }

    #[setter]
    fn set_onload(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        event_content_handlers::set_element_handler(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "load",
            callback,
        );
    }

    #[getter]
    fn onerror(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        event_content_handlers::element_handler_value(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "error",
        )
    }

    #[setter]
    fn set_onerror(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        event_content_handlers::set_element_handler(
            ctx,
            &self.base.base.realm,
            self.base.base.id,
            &this.0,
            "error",
            callback,
        );
    }
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_html_element(ctx, this.0)
    }

    #[getter]
    fn dataset(&self, ctx: &mut Ctx) -> OpResult<Value> {
        dataset::for_element(ctx, &self.base.base)
    }

    #[getter]
    fn validity(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let node = &self.base.base;
        if let Some(value) = node
            .collections
            .borrow()
            .get("validity")
            .and_then(WeakValue::upgrade)
        {
            return Ok(value);
        }
        let value = forms::validity_object(ctx, this.0)?;
        node.collections.borrow_mut().insert(
            "validity".into(),
            ctx.weak_value(&value).expect("validity state wrapper"),
        );
        Ok(value)
    }

    #[getter]
    fn will_validate(&self) -> bool {
        forms::will_validate(&self.base.base.realm, self.base.base.id)
    }

    #[getter]
    fn validation_message(&self) -> OpResult<String> {
        let node = &self.base.base;
        forms::validation_message(&node.realm, node.id, &node.realm.forms)
    }

    #[method(coerce)]
    fn set_custom_validity(&self, message: &str) {
        let node = &self.base.base;
        forms::set_custom_validity(&mut node.realm.forms.borrow_mut(), node.id, message);
    }

    fn check_validity(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let node = &self.base.base;
        forms::check_validity(ctx, &node.realm, node.id, &node.realm.forms)
    }

    fn report_validity(&self, ctx: &mut Ctx) -> OpResult<bool> {
        let node = &self.base.base;
        forms::report_validity(ctx, &node.realm, node.id, &node.realm.forms)
    }
    #[getter]
    fn onkeydown(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "keydown")
    }
    #[setter]
    fn set_onkeydown(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "keydown", callback);
    }
    #[getter]
    fn onkeyup(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "keyup")
    }
    #[setter]
    fn set_onkeyup(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "keyup", callback);
    }
    #[getter]
    fn onkeypress(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "keypress")
    }
    #[setter]
    fn set_onkeypress(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "keypress", callback);
    }
    #[getter]
    fn onbeforeinput(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "beforeinput")
    }
    #[setter]
    fn set_onbeforeinput(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "beforeinput", callback);
    }
    #[getter]
    fn oninvalid(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "invalid")
    }
    #[setter]
    fn set_oninvalid(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "invalid", callback);
    }
    #[getter]
    fn onchange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "change")
    }
    #[setter]
    fn set_onchange(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "change", callback);
    }
    #[getter]
    fn onfocus(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "focus")
    }
    #[setter]
    fn set_onfocus(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "focus", callback);
    }
    #[getter]
    fn onblur(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "blur")
    }
    #[setter]
    fn set_onblur(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "blur", callback);
    }
    #[getter]
    fn onfocusin(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "focusin")
    }
    #[setter]
    fn set_onfocusin(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "focusin", callback);
    }
    #[getter]
    fn onfocusout(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "focusout")
    }
    #[setter]
    fn set_onfocusout(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "focusout", callback);
    }
    #[getter]
    fn ondblclick(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "dblclick")
    }
    #[setter]
    fn set_ondblclick(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "dblclick", callback);
    }
    #[getter]
    fn onpointerdown(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "pointerdown")
    }
    #[setter]
    fn set_onpointerdown(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "pointerdown", callback);
    }
    #[getter]
    fn onpointerup(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "pointerup")
    }
    #[setter]
    fn set_onpointerup(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "pointerup", callback);
    }
    #[getter]
    fn onpointermove(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "pointermove")
    }
    #[setter]
    fn set_onpointermove(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "pointermove", callback);
    }
    #[getter]
    fn onpointercancel(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "pointercancel")
    }
    #[setter]
    fn set_onpointercancel(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "pointercancel", callback);
    }
    #[getter]
    fn onmousedown(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "mousedown")
    }
    #[setter]
    fn set_onmousedown(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mousedown", callback);
    }
    #[getter]
    fn onmouseup(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "mouseup")
    }
    #[setter]
    fn set_onmouseup(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mouseup", callback);
    }
    #[getter]
    fn onmousemove(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "mousemove")
    }
    #[setter]
    fn set_onmousemove(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mousemove", callback);
    }
    #[getter]
    fn onmouseover(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "mouseover")
    }
    #[setter]
    fn set_onmouseover(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mouseover", callback);
    }
    #[getter]
    fn onmouseout(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "mouseout")
    }
    #[setter]
    fn set_onmouseout(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mouseout", callback);
    }
    #[getter]
    fn onmouseenter(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "mouseenter")
    }
    #[setter]
    fn set_onmouseenter(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mouseenter", callback);
    }
    #[getter]
    fn onmouseleave(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "mouseleave")
    }
    #[setter]
    fn set_onmouseleave(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "mouseleave", callback);
    }
    #[getter]
    fn onwheel(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "wheel")
    }
    #[setter]
    fn set_onwheel(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "wheel", callback);
    }
    #[getter]
    fn onscroll(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "scroll")
    }
    #[setter]
    fn set_onscroll(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "scroll", callback);
    }
    #[getter]
    fn oncompositionstart(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "compositionstart")
    }
    #[setter]
    fn set_oncompositionstart(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "compositionstart", callback);
    }
    #[getter]
    fn oncompositionupdate(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "compositionupdate")
    }
    #[setter]
    fn set_oncompositionupdate(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "compositionupdate", callback);
    }
    #[getter]
    fn oncompositionend(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "compositionend")
    }
    #[setter]
    fn set_oncompositionend(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "compositionend", callback);
    }
    #[getter]
    fn ontouchstart(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "touchstart")
    }
    #[setter]
    fn set_ontouchstart(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "touchstart", callback);
    }
    #[getter]
    fn ontouchend(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "touchend")
    }
    #[setter]
    fn set_ontouchend(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "touchend", callback);
    }
    #[getter]
    fn ontouchmove(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "touchmove")
    }
    #[setter]
    fn set_ontouchmove(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "touchmove", callback);
    }
    #[getter]
    fn ontouchcancel(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base
            .base
            .base
            .handler_value(ctx, &this.0, "touchcancel")
    }
    #[setter]
    fn set_ontouchcancel(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "touchcancel", callback);
    }
    #[getter]
    fn oninput(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "input")
    }
    #[setter]
    fn set_oninput(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "input", callback);
    }
    #[getter]
    fn onclick(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.base.base.handler_value(ctx, &this.0, "click")
    }
    #[setter]
    fn set_onclick(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .base
            .base
            .set_handler(ctx, &this.0, "click", callback);
    }
    #[getter]
    fn tab_index(&self) -> OpResult<i32> {
        if let Some(value) = self.base.base.get_null_attribute("tabindex")? {
            if let Ok(value) = value.parse() {
                return Ok(value);
            }
        }
        let name = self.base.base.local_name()?.unwrap_or_default();
        Ok(
            if matches!(
                name.as_str(),
                "input" | "button" | "textarea" | "select" | "summary"
            ) || (matches!(name.as_str(), "a" | "area")
                && self.base.base.has_null_attribute("href")?)
            {
                0
            } else {
                -1
            },
        )
    }
    #[setter]
    fn set_tab_index(&self, index: i32) -> OpResult<()> {
        self.base
            .base
            .set_attribute_core("tabindex", &index.to_string())
    }
    fn focus(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomHtmlElement, _>(&this.0, |element| {
            (element.base.base.realm.clone(), element.base.base.id)
        })?;
        realm.focus(ctx, Some(node))
    }
    fn blur(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let (realm, node) = ctx.with_instance::<DomHtmlElement, _>(&this.0, |element| {
            (element.base.base.realm.clone(), element.base.base.id)
        })?;
        if realm.focused_node() == Some(node) {
            realm.focus(ctx, None)?;
        }
        Ok(())
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        if let Some(value) = self.base.base.get_null_attribute("value")? {
            return Ok(value);
        }
        if self.base.base.local_name()?.as_deref() == Some("textarea") {
            return Ok(self.base.base.text_content()?.unwrap_or_default());
        }
        Ok(String::new())
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base;
        let local_name = node.local_name()?;
        if matches!(local_name.as_deref(), Some("input" | "textarea")) {
            node.realm.set_control_value_and_selection(
                node.id,
                value,
                local_name.as_deref() == Some("textarea"),
            )
        } else {
            forms::set_control_value(
                &node.realm,
                &mut node.realm.forms.borrow_mut(),
                node.id,
                value,
            )
        }
    }
    #[getter]
    fn selection_start(&self) -> OpResult<Option<usize>> {
        let node = &self.base.base;
        if !node.realm.supports_text_selection(node.id) {
            return Ok(None);
        }
        Ok(Some(node.realm.selection(node.id)?.0))
    }
    #[setter]
    fn set_selection_start(&self, ctx: &mut Ctx, start: usize) -> OpResult<()> {
        let node = &self.base.base;
        let (_, end, direction) = node.realm.selection(node.id)?;
        node.realm
            .set_selection_and_queue_event(ctx, node.id, start, end.max(start), &direction)
    }
    #[getter]
    fn selection_end(&self) -> OpResult<Option<usize>> {
        let node = &self.base.base;
        if !node.realm.supports_text_selection(node.id) {
            return Ok(None);
        }
        Ok(Some(node.realm.selection(node.id)?.1))
    }
    #[setter]
    fn set_selection_end(&self, ctx: &mut Ctx, end: usize) -> OpResult<()> {
        let node = &self.base.base;
        let (start, _, direction) = node.realm.selection(node.id)?;
        node.realm
            .set_selection_and_queue_event(ctx, node.id, start, end, &direction)
    }
    #[getter]
    fn selection_direction(&self) -> OpResult<Option<String>> {
        let node = &self.base.base;
        if !node.realm.supports_text_selection(node.id) {
            return Ok(None);
        }
        Ok(Some(node.realm.selection(node.id)?.2))
    }
    #[setter(coerce)]
    fn set_selection_direction(&self, ctx: &mut Ctx, direction: &str) -> OpResult<()> {
        let node = &self.base.base;
        let (start, end, _) = node.realm.selection(node.id)?;
        node.realm
            .set_selection_and_queue_event(ctx, node.id, start, end, direction)
    }
    #[method(coerce)]
    fn set_selection_range(
        &self,
        ctx: &mut Ctx,
        start: usize,
        end: usize,
        direction: Option<String>,
    ) -> OpResult<()> {
        let node = &self.base.base;
        node.realm.set_selection_and_queue_event(
            ctx,
            node.id,
            start,
            end,
            direction.as_deref().unwrap_or("none"),
        )
    }
    #[method(coerce)]
    fn set_range_text(
        &self,
        ctx: &mut Ctx,
        replacement: &str,
        start: lumen_bind::Passed<usize>,
        end: lumen_bind::Passed<usize>,
        selection_mode: lumen_bind::Passed<Value>,
    ) -> OpResult<()> {
        let mode = match selection_mode.0 {
            None | Some(Value::Undefined) => "preserve".to_owned(),
            Some(value) => ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string(),
        };
        let use_current_selection = start.0.is_none() && end.0.is_none();
        let node = &self.base.base;
        node.realm.set_range_text(
            ctx,
            node.id,
            replacement,
            start.0,
            end.0,
            use_current_selection,
            &mode,
        )
    }
    fn select(&self, ctx: &mut Ctx) -> OpResult<()> {
        let node = &self.base.base;
        if !node.realm.supports_text_selection(node.id) {
            return Ok(());
        }
        let value = node.realm.control_value(node.id)?;
        let end = lumen_common::smuggle::utf16_unit_len(&value);
        node.realm
            .set_selection_and_queue_event(ctx, node.id, 0, end, "none")
    }
    #[getter]
    fn checked(&self) -> OpResult<bool> {
        forms::checked(&self.base.base.realm, self.base.base.id)
    }
    #[setter(coerce)]
    fn set_checked(&self, checked: bool) -> OpResult<()> {
        forms::set_checked(
            &self.base.base.realm,
            &mut self.base.base.realm.forms.borrow_mut(),
            self.base.base.id,
            checked,
        )
    }
    #[getter]
    fn indeterminate(&self) -> OpResult<bool> {
        Ok(forms::indeterminate(
            &self.base.base.realm.forms.borrow(),
            self.base.base.id,
        ))
    }
    #[setter(coerce)]
    fn set_indeterminate(&self, indeterminate: bool) -> OpResult<()> {
        forms::set_indeterminate(
            &mut self.base.base.realm.forms.borrow_mut(),
            self.base.base.id,
            indeterminate,
        );
        Ok(())
    }
    #[getter]
    fn disabled(&self) -> OpResult<bool> {
        self.base.base.has_null_attribute("disabled")
    }
    #[setter(coerce)]
    fn set_disabled(&self, disabled: bool) -> OpResult<()> {
        if disabled {
            self.base.base.set_attribute_core("disabled", "")
        } else {
            self.base.base.remove_attribute_core("disabled")
        }
    }
}

#[lumen_bind::class(name = "CharacterData", extends = DomNode, hint(js(webidl)))]
pub struct DomCharacterData {
    base: DomNode,
}
#[lumen_bind::methods]
impl DomCharacterData {
    #[getter]
    fn data(&self) -> OpResult<String> {
        Ok(self.base.node_value()?.unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_data(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        if matches!(value, Value::Null) {
            return self.base.set_node_value(Some(""));
        }
        let value = ctx.coerce_string(&value).map_err(OpError::thrown)?;
        self.base.set_node_value(Some(&value))
    }
    #[getter]
    fn length(&self) -> OpResult<usize> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .character_data_length(self.base.id)
            .map_err(dom_error)
    }

    #[method(name = "substringData", coerce)]
    fn substring_data(&self, offset: u32, count: u32) -> OpResult<String> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .substring_data(self.base.id, offset as usize, count as usize)
            .map_err(dom_error)
    }

    #[method(name = "appendData", coerce)]
    fn append_data(&self, data: &str) -> OpResult<()> {
        self.mutate_data(|document| document.append_data(self.base.id, data))
    }

    #[method(name = "insertData", coerce)]
    fn insert_data(&self, offset: u32, data: &str) -> OpResult<()> {
        self.mutate_data(|document| document.insert_data(self.base.id, offset as usize, data))
    }

    #[method(name = "deleteData", coerce)]
    fn delete_data(&self, offset: u32, count: u32) -> OpResult<()> {
        self.mutate_data(|document| {
            document.delete_data(self.base.id, offset as usize, count as usize)
        })
    }

    #[method(name = "replaceData", coerce)]
    fn replace_data(&self, offset: u32, count: u32, data: &str) -> OpResult<()> {
        self.mutate_data(|document| {
            document.replace_data_range(self.base.id, offset as usize, count as usize, data)
        })
    }
}

impl DomCharacterData {
    fn mutate_data(
        &self,
        mutate: impl FnOnce(&mut lumen_html::Document) -> Result<(), Error>,
    ) -> OpResult<()> {
        let result = mutate(self.base.realm.session.borrow_mut().document_mut()).map_err(dom_error);
        if result.is_ok() {
            self.base.realm.invalidate_textarea_ancestor(self.base.id);
        }
        result
    }
}

#[lumen_bind::class(name = "Text", extends = DomCharacterData, hint(js(webidl)))]
pub struct DomText {
    base: DomCharacterData,
}
#[lumen_bind::methods]
impl DomText {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, #[default("")] data: &str) -> OpResult<NodeConstructorResult<Self>> {
        let node = construct_document_node(ctx, NodeKind::Text(data.into()))?;
        Ok(NodeConstructorResult::new(node, |base| Self {
            base: DomCharacterData { base },
        }))
    }
}

#[lumen_bind::class(name = "CDATASection", extends = DomText, hint(js(webidl)))]
pub struct DomCDataSection {
    base: DomText,
}
#[lumen_bind::methods]
impl DomCDataSection {}

#[lumen_bind::class(name = "Comment", extends = DomCharacterData, hint(js(webidl)))]
pub struct DomComment {
    base: DomCharacterData,
}
#[lumen_bind::methods]
impl DomComment {
    #[constructor(coerce)]
    fn new(ctx: &mut Ctx, #[default("")] data: &str) -> OpResult<NodeConstructorResult<Self>> {
        let node = construct_document_node(ctx, NodeKind::Comment(data.into()))?;
        Ok(NodeConstructorResult::new(node, |base| Self {
            base: DomCharacterData { base },
        }))
    }
}

#[lumen_bind::class(name = "ProcessingInstruction", extends = DomCharacterData, hint(js(webidl)))]
pub struct DomProcessingInstruction {
    base: DomCharacterData,
}
#[lumen_bind::methods]
impl DomProcessingInstruction {
    #[getter]
    fn target(&self) -> OpResult<String> {
        let session = self.base.base.realm.session.borrow();
        match session
            .document()
            .kind(self.base.base.id)
            .map_err(dom_error)?
        {
            NodeKind::ProcessingInstruction { target, .. } => Ok(target.clone()),
            _ => Err(OpError::new(
                "TypeError",
                "target requires a ProcessingInstruction",
            )),
        }
    }
}

#[lumen_bind::class(name = "DocumentType", extends = DomNode, hint(js(webidl)))]
pub struct DomDocumentType {
    base: DomNode,
}
#[lumen_bind::methods]
impl DomDocumentType {
    #[getter]
    fn name(&self) -> OpResult<String> {
        let session = self.base.realm.session.borrow();
        match session.document().kind(self.base.id).map_err(dom_error)? {
            NodeKind::DocumentType(name) => Ok(name.clone()),
            _ => Err(OpError::new("TypeError", "doctype requires a DocumentType")),
        }
    }

    #[getter(name = "publicId")]
    fn public_id(&self) -> OpResult<String> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .doctype_public_id(self.base.id)
            .map(str::to_owned)
            .map_err(dom_error)
    }

    #[getter(name = "systemId")]
    fn system_id(&self) -> OpResult<String> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .doctype_system_id(self.base.id)
            .map(str::to_owned)
            .map_err(dom_error)
    }
}

#[lumen_bind::class(name = "DocumentFragment", extends = DomNode, hint(js(webidl)))]
pub struct DomDocumentFragment {
    base: DomNode,
}

/// Node constructors use the relevant Window's associated document, just as
/// document factories do, and register the constructor's own wrapper so later
/// tree operations preserve its JavaScript identity.
fn construct_document_node(ctx: &mut Ctx, kind: NodeKind) -> OpResult<DomNode> {
    let _html_allocations = enter_html_allocation_category();
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("TypeError", "node constructor has no associated document"))?;
    let id = realm
        .session
        .borrow_mut()
        .document_mut()
        .create(kind)
        .map_err(dom_error)?;
    Ok(DomNode {
        base: DomEventTarget::node(&realm, id),
        realm,
        id,
        collections: RefCell::new(HashMap::new()),
    })
}

pub(crate) struct NodeConstructorResult<T> {
    native: T,
    realm: Rc<DomRealm>,
    id: NodeId,
}

impl<T> NodeConstructorResult<T> {
    pub(crate) fn new(node: DomNode, make: impl FnOnce(DomNode) -> T) -> Self {
        Self {
            realm: node.realm.clone(),
            id: node.id,
            native: make(node),
        }
    }

    pub(crate) fn from_native(native: T, realm: Rc<DomRealm>, id: NodeId) -> Self {
        Self { native, realm, id }
    }
}

impl<T: lumen_bind::Class> lumen_bind::CtorRet<lumen::embed::JsHost, T>
    for NodeConstructorResult<T>
{
    fn into_ctor(
        self,
        cx: &<lumen::embed::JsHost as lumen_bind::Host>::Cx<'_>,
    ) -> Result<Value, Value> {
        let _html_allocations = enter_html_allocation_category();
        let value = match <lumen::embed::JsHost as lumen_bind::Host>::construct(cx, self.native) {
            Ok(value) => value,
            Err(error) => {
                let _ = self
                    .realm
                    .session
                    .borrow_mut()
                    .document_mut()
                    .destroy_subtree(self.id);
                return Err(error);
            }
        };
        // Ordinary native construction allocates the final object after the
        // typed constructor returns. Cache that object, including subclass
        // prototypes, rather than the provisional constructor receiver.
        <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| {
            ctx.set_native_identity_owner::<DomNode>(&value)
                .map_err(|error| error.to_value(ctx))?;
            if let Some(weak) = ctx.weak_value(&value) {
                self.realm.wrappers.borrow_mut().insert(self.id, weak);
            }
            Ok(())
        })?;
        Ok(value)
    }
}

#[lumen_bind::methods]
impl DomDocumentFragment {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<NodeConstructorResult<Self>> {
        let node = construct_document_node(ctx, NodeKind::DocumentFragment)?;
        Ok(NodeConstructorResult::new(node, |base| Self { base }))
    }

    #[method(coerce)]
    fn get_element_by_id(&self, ctx: &mut Ctx, id: &str) -> OpResult<Value> {
        let found = element_by_id_in(
            self.base.realm.session.borrow().document(),
            self.base.id,
            id,
        )
        .map_err(dom_error)?;
        Ok(self.base.realm.wrap_option(ctx, found))
    }
}

#[lumen_bind::class(name = "Document", extends = DomNode, hint(js(webidl)))]
pub struct DomDocument {
    base: DomNode,
    realm: Rc<DomRealm>,
    fonts: RefCell<Option<Value>>,
}

#[lumen_bind::class(name = "XMLDocument", extends = DomDocument, hint(js(webidl)))]
pub struct DomXmlDocument {
    base: DomDocument,
}
#[lumen_bind::methods]
impl DomXmlDocument {}

#[lumen_bind::methods]
impl DomDocument {
    #[method(name = "elementFromPoint", coerce)]
    fn element_from_point(&self, ctx: &mut Ctx, x: f64, y: f64) -> OpResult<Value> {
        let node = geometry::element_from_point_in_tree(&self.realm, None, x, y)?;
        Ok(self.realm.wrap_option(ctx, node))
    }

    #[method(name = "elementsFromPoint", coerce)]
    fn elements_from_point(&self, ctx: &mut Ctx, x: f64, y: f64) -> OpResult<Vec<Value>> {
        let nodes = geometry::elements_from_point(&self.realm, x, y)?;
        Ok(nodes
            .into_iter()
            .map(|node| self.realm.wrap(ctx, node))
            .collect())
    }

    #[constructor]
    fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Self> {
        let document = lumen_html::Document::new(100_000);
        let realm =
            DomRealm::realm_from_document_with_metadata(document, "application/xml", false, false);
        realm.set_document_url("about:blank");
        realm.set_document_origin(
            browsing_context::invocation_origin(ctx)
                .unwrap_or_else(browsing_context::Origin::opaque),
        );
        let root = realm.session.borrow().document().root();
        let value = this.0;
        if let Some(wrapper) = ctx.weak_value(&value) {
            *realm.document_wrapper.borrow_mut() = Some(wrapper);
        }
        Ok(Self {
            base: DomNode {
                base: DomEventTarget::node(&realm, root),
                realm: realm.clone(),
                id: root,
                collections: RefCell::new(HashMap::new()),
            },
            realm,
            fonts: RefCell::new(None),
        })
    }

    #[getter]
    fn cookie(&self) -> OpResult<String> {
        self.cookie_value()
    }

    #[getter]
    fn forms(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        self.base.descendant_collection(
            ctx,
            this.0,
            "forms".into(),
            DescendantFilter::Tag("form".into()),
        )
    }

    #[setter(coerce)]
    fn set_cookie(&self, assignment: &str) -> OpResult<()> {
        self.set_cookie_value(assignment)
    }

    #[getter]
    fn fonts(&self, ctx: &mut Ctx) -> Value {
        if let Some(value) = self.fonts.borrow().as_ref() {
            return value.clone();
        }
        let value = ctx.new_instance(font_loading::DomFontFaceSet::new(&self.realm));
        ctx.instance_data::<font_loading::DomFontFaceSet>(&value)
            .expect("new FontFaceSet has native backing")
            .borrow()
            .attach(ctx, &value);
        *self.fonts.borrow_mut() = Some(value.clone());
        value
    }

    #[getter]
    fn head(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let session = self.base.realm.session.borrow();
        let document = session.document();
        let html = named_child(document, document.root(), "html").map_err(dom_error)?;
        let id = html
            .map(|html| named_child(document, html, "head"))
            .transpose()
            .map_err(dom_error)?
            .flatten();
        drop(session);
        Ok(self.base.realm.wrap_option(ctx, id))
    }

    #[getter]
    fn title(&self) -> OpResult<String> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .title()
            .map_err(dom_error)
    }

    #[setter(coerce)]
    fn set_title(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let title = self
            .base
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .ensure_title_element()
            .map_err(dom_error)?;
        if let Some(title) = title {
            DomNode::replace_text_content(ctx, &self.base.realm, title, value)?;
        }
        Ok(())
    }

    #[method(name = "open")]
    fn open(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.realm.open_document_stream(ctx)?;
        Ok(this.0)
    }

    #[method(name = "write", coerce)]
    fn write(&self, ctx: &mut Ctx, #[varargs] chunks: Vec<String>) -> OpResult<()> {
        self.realm.write_document_stream(ctx, &chunks, false)
    }

    #[method(name = "writeln", coerce)]
    fn writeln(&self, ctx: &mut Ctx, #[varargs] chunks: Vec<String>) -> OpResult<()> {
        self.realm.write_document_stream(ctx, &chunks, true)
    }

    #[method(name = "close")]
    fn close(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.realm.close_document_stream(ctx)
    }

    #[getter(rename(js = "URL"))]
    fn document_url(&self) -> String {
        self.base
            .realm
            .document_url()
            .unwrap_or_else(|| "about:blank".into())
    }
    #[getter(rename(js = "documentURI"))]
    fn document_uri(&self) -> String {
        self.document_url()
    }
    #[method(name = "getElementsByTagName", coerce)]
    fn get_elements_by_tag_name(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        qualified_name: &str,
    ) -> Value {
        self.base.descendant_collection(
            ctx,
            this.0,
            format!("tag:{}:{qualified_name}", qualified_name.len()),
            DescendantFilter::Tag(qualified_name.to_owned()),
        )
    }

    #[method(name = "getElementsByTagNameNS", coerce)]
    fn get_elements_by_tag_name_ns(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> Value {
        let namespace_uri = namespace_uri
            .filter(|namespace_uri| !namespace_uri.is_empty())
            .map(str::to_owned);
        let key = tag_ns_collection_key(namespace_uri.as_deref(), local_name);
        self.base.descendant_collection(
            ctx,
            this.0,
            key,
            DescendantFilter::TagNs(namespace_uri, local_name.to_owned()),
        )
    }

    #[method(name = "getElementsByClassName", coerce)]
    fn get_elements_by_class_name(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        class_names: &str,
    ) -> Value {
        let key = format!("class:{}:{class_names}", class_names.len());
        let names = html_space_tokens(class_names).map(str::to_owned).collect();
        self.base
            .descendant_collection(ctx, this.0, key, DescendantFilter::Class(names))
    }

    #[method(name = "getElementsByName", coerce)]
    fn get_elements_by_name(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        name: &str,
    ) -> Value {
        ctx.new_instance(DomNodeList::descendants(
            self.realm.clone(),
            self.base.id,
            DescendantFilter::Name(name.to_owned()),
            this.0,
        ))
    }

    #[getter(name = "defaultView")]
    fn default_view(&self) -> Value {
        self.realm
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
            .unwrap_or(Value::Null)
    }

    #[getter(name = "readyState")]
    fn ready_state(&self) -> &'static str {
        self.realm.ready_state.get().as_str()
    }

    #[getter(name = "currentScript")]
    fn current_script(&self, ctx: &mut Ctx) -> Value {
        self.realm
            .current_script
            .get()
            .map(|node| {
                let (realm, node) = self.realm.resolve_adopted_node(node);
                realm.wrap(ctx, node)
            })
            .unwrap_or(Value::Null)
    }

    #[getter(name = "compatMode")]
    fn compat_mode(&self) -> &'static str {
        self.realm.session.borrow().document().compat_mode()
    }

    #[getter]
    fn doctype(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let doctype = self
            .realm
            .session
            .borrow()
            .document()
            .doctype()
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, doctype))
    }

    #[getter]
    fn implementation(&self, ctx: &mut Ctx) -> Value {
        if let Some(value) = self
            .realm
            .implementation_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomImplementation {
            realm: self.realm.clone(),
        });
        *self.realm.implementation_wrapper.borrow_mut() = ctx.weak_value(&value);
        value
    }

    #[getter(name = "contentType")]
    fn content_type(&self) -> String {
        self.realm.content_type.clone()
    }

    #[getter(name = "characterSet")]
    fn character_set(&self) -> &'static str {
        self.realm.document_encoding()
    }

    #[getter]
    fn charset(&self) -> &'static str {
        self.realm.document_encoding()
    }

    #[getter(name = "inputEncoding")]
    fn input_encoding(&self) -> &'static str {
        self.realm.document_encoding()
    }

    #[getter]
    fn location(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let Some(context) = self.realm.browsing_context() else {
            return Ok(Value::Null);
        };
        if !browsing_context::context_document(&context)
            .is_some_and(|document| Rc::ptr_eq(&document, &self.realm))
        {
            // A retained Document from a replaced child realm no longer owns
            // the active browsing context's location object.
            return Ok(Value::Null);
        }
        let realm = browsing_context::context_realm_handle(&context);
        ctx.with_host_realm(&realm, |ctx| {
            window_globals::location_value(ctx, &self.realm, &context)
        })
        .map_err(browsing_context::host_realm_error)?
    }

    #[setter(coerce)]
    fn set_location(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let Some(context) = self.realm.browsing_context() else {
            return Err(OpError::new(
                "InvalidStateError",
                "Document has no active browsing context",
            ));
        };
        if !browsing_context::context_document(&context)
            .is_some_and(|document| Rc::ptr_eq(&document, &self.realm))
        {
            return Err(OpError::new(
                "InvalidStateError",
                "Document no longer owns the active browsing context",
            ));
        }
        let entry_base = window_globals::entry_base_url(ctx, &context);
        context.request_location_navigation_from(value, &entry_base)
    }

    #[getter]
    fn fullscreen_enabled(&self) -> bool {
        presentation::fullscreen_enabled(&self.realm)
    }

    #[getter]
    fn fullscreen_element(&self, ctx: &mut Ctx) -> Value {
        self.realm.wrap_option(ctx, self.realm.fullscreen_element())
    }

    #[getter]
    fn pointer_lock_element(&self, ctx: &mut Ctx) -> Value {
        self.realm
            .wrap_option(ctx, self.realm.pointer_lock_element())
    }

    fn exit_fullscreen(&self, ctx: &mut Ctx) -> lumen::embed::Promise<()> {
        lumen::embed::Promise::ready(presentation::exit_fullscreen(ctx, &self.realm))
    }

    fn exit_pointer_lock(&self, ctx: &mut Ctx) -> OpResult<()> {
        presentation::exit_pointer_lock(ctx, &self.realm)
    }

    fn import_node(
        &self,
        ctx: &mut Ctx,
        node: &DomNode,
        options: Option<Value>,
    ) -> OpResult<Value> {
        let deep = match options {
            None | Some(Value::Undefined) => false,
            Some(Value::Obj(value)) => {
                let options = Value::Obj(value);
                let registry = ctx
                    .member_get(&options, "customElementRegistry")
                    .map_err(OpError::thrown)?;
                if !matches!(registry, Value::Undefined) {
                    return Err(OpError::new(
                        "NotSupportedError",
                        "importing with an explicit custom element registry is not implemented",
                    ));
                }
                let self_only = ctx
                    .member_get(&options, "selfOnly")
                    .map_err(OpError::thrown)?;
                !ctx.to_boolean(&self_only)
            }
            Some(Value::Null) => true,
            Some(value) => ctx.to_boolean(&value),
        };
        {
            let source = node.realm.session.borrow();
            let document = source.document();
            if matches!(
                document.kind(node.id).map_err(dom_error)?,
                NodeKind::Document
            ) || document.shadow_host(node.id).map_err(dom_error)?.is_some()
            {
                return Err(OpError::new(
                    "NotSupportedError",
                    "documents and shadow roots cannot be imported",
                ));
            }
        }
        let id = if Rc::ptr_eq(&self.realm, &node.realm) {
            let mut target = self.realm.session.borrow_mut();
            target
                .document_mut()
                .clone_node(node.id, deep)
                .map_err(dom_error)?
        } else {
            let source = node.realm.session.borrow();
            self.realm
                .session
                .borrow_mut()
                .document_mut()
                .clone_subtree_from(source.document(), node.id, deep)
                .map_err(dom_error)?
        };
        let script_pairs = {
            let source = node.realm.session.borrow();
            let target = self.realm.session.borrow();
            script_loading::paired_subtree_nodes(source.document(), target.document(), node.id, id)
        };
        let form_state = node.realm.forms.borrow().clone();
        let target_session = self.realm.session.borrow();
        forms::clone_live_values_into(
            &form_state,
            &mut self.realm.forms.borrow_mut(),
            target_session.document(),
            &script_pairs,
        );
        drop(target_session);
        if Rc::ptr_eq(&self.realm, &node.realm) {
            self.realm.scripts.borrow_mut().clone_states(&script_pairs);
        } else {
            let source_scripts = node.realm.scripts.borrow();
            self.realm
                .scripts
                .borrow_mut()
                .clone_states_from(&source_scripts, &script_pairs);
        }
        event_content_handlers::initialize_subtree(ctx, &self.realm, id)?;
        Ok(self.realm.wrap(ctx, id))
    }

    fn adopt_node(&self, ctx: &mut Ctx, node: Value) -> OpResult<Value> {
        // Do not keep lumen-bind's borrowed `&DomNode` argument projection alive
        // while migrating the native wrapper: adoption mutates that very object
        // in place so it can preserve JavaScript identity.
        let (source_realm, source_id) =
            ctx.with_instance::<DomNode, _>(&node, |node| (node.realm.clone(), node.id))?;
        let adopted = DomRealm::adopt_node_from(&self.realm, ctx, &source_realm, source_id)?;
        Ok(self.realm.wrap(ctx, adopted))
    }

    #[getter]
    fn adopted_style_sheets(&self, ctx: &mut Ctx) -> OpResult<Value> {
        cssom::adopted_stylesheets(ctx, &self.realm, None)
    }
    #[setter]
    fn set_adopted_style_sheets(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        cssom::set_adopted_stylesheets(ctx, &self.realm, None, value)
    }
    #[getter]
    fn style_sheets(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        cssom::document_style_sheets(ctx, &self.realm, self.base.id, this.0)
    }
    #[getter]
    fn timeline(&self, ctx: &mut Ctx) -> OpResult<Value> {
        animations::document_timeline(ctx, &self.realm)
    }
    fn get_animations(&self, ctx: &mut Ctx) -> OpResult<Value> {
        animations::for_tree_root(ctx, &self.realm, self.base.id)
    }
    fn create_range(&self, ctx: &mut Ctx) -> Value {
        ctx.new_instance(range::DomRange::new(
            self.realm.clone(),
            self.realm.ranges.clone(),
        ))
    }

    fn get_selection(&self, ctx: &mut Ctx) -> Value {
        self.realm.selection_value(ctx)
    }
    fn create_tree_walker(
        &self,
        ctx: &mut Ctx,
        root: &DomNode,
        what_to_show: Option<u32>,
        filter: Option<Value>,
    ) -> OpResult<Value> {
        if !Rc::ptr_eq(&self.realm, &root.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "root belongs to another document",
            ));
        }
        Ok(ctx.new_instance(document_utilities::DomTreeWalker::new(
            self.realm.clone(),
            root.id,
            what_to_show,
            filter,
            self.realm.tree_walkers.clone(),
        )))
    }

    fn create_node_iterator(
        &self,
        ctx: &mut Ctx,
        root: &DomNode,
        what_to_show: Option<u32>,
        filter: Option<Value>,
    ) -> OpResult<Value> {
        if !Rc::ptr_eq(&self.realm, &root.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "root belongs to another document",
            ));
        }
        Ok(ctx.new_instance(document_utilities::DomNodeIterator::new(
            self.realm.clone(),
            root.id,
            what_to_show,
            filter,
            self.realm.iterators.clone(),
        )))
    }

    #[getter]
    fn oninput(&self) -> Option<lumen::embed::JsFunction> {
        self.base.base.handler("input")
    }
    #[setter]
    fn set_oninput(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.base.set_handler(ctx, &this.0, "input", callback);
    }
    #[getter]
    fn active_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if let Some(node) = self.realm.focused_node() {
            let target = self
                .realm
                .session
                .borrow()
                .document()
                .retarget(node, Some(self.base.id))
                .map_err(dom_error)?;
            return Ok(self.realm.wrap(ctx, target));
        }
        self.body(ctx)
    }

    #[method(name = "hasFocus")]
    fn has_focus(&self) -> bool {
        self.realm
            .browsing_context()
            .is_some_and(|context| browsing_context::is_active_document(&context, &self.realm))
    }

    #[getter]
    fn document_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let id = selector::document_element(self.realm.session.borrow().document());
        Ok(self.realm.wrap_option(ctx, id))
    }

    #[getter]
    fn dir(&self) -> String {
        let session = self.realm.session.borrow();
        let document = session.document();
        selector::document_element(document)
            .filter(|id| lumen_html::forms::html_element_local_name(document, *id) == Some("html"))
            .and_then(|id| lumen_html::directionality::dir_attribute_state(document, id))
            .map_or("", lumen_html::directionality::DirAttributeState::keyword)
            .to_owned()
    }

    #[setter(coerce)]
    fn set_dir(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let id = {
            let session = self.realm.session.borrow();
            let document = session.document();
            selector::document_element(document).filter(|id| {
                lumen_html::forms::html_element_local_name(document, *id) == Some("html")
            })
        };
        let Some(id) = id else {
            return Ok(());
        };
        let element = self.realm.wrap(ctx, id);
        ctx.with_instance::<DomNode, _>(&element, |node| node.set_attribute_core("dir", value))?
    }

    #[getter]
    fn scrolling_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let id = scrolling::document_scrolling_element(&self.realm)?;
        Ok(self.realm.wrap_option(ctx, id))
    }

    #[getter]
    fn body(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let html = named_child(document, document.root(), "html").map_err(dom_error)?;
        let body = if let Some(html) = html {
            named_child(document, html, "body").map_err(dom_error)?
        } else {
            None
        };
        drop(session);
        Ok(self.realm.wrap_option(ctx, body))
    }

    #[method(coerce)]
    fn create_element(&self, ctx: &mut Ctx, name: &str, options: Option<Value>) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        if !lumen_html::xml::is_xml_name(name) {
            return Err(OpError::new(
                "InvalidCharacterError",
                "invalid element name",
            ));
        }
        let is = custom_elements::custom_element_is(ctx, options)?;
        let (namespace, name) = if self.realm.is_html_document {
            (Namespace::Html, name.to_ascii_lowercase())
        } else if self.realm.content_type == "application/xhtml+xml" {
            // XHTML is parsed as XML, so element names remain case-sensitive,
            // while unnamespaced createElement calls still use the HTML namespace.
            (Namespace::Html, name.to_owned())
        } else {
            (Namespace::Other(Rc::from("")), name.to_owned())
        };
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace,
                name: name.into(),
                attributes: is
                    .map(|value| vec![("is".into(), value)])
                    .unwrap_or_default(),
            })
            .map_err(dom_error)?;
        custom_elements::upgrade_created_element(ctx, &self.realm, id)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(name = "createElementNS", coerce)]
    fn create_element_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        name: &str,
    ) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let namespace = namespace_for_qname(ctx, namespace_uri, name)?;
        let name: lumen_html::Name = name.into();
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace,
                name,
                attributes: Vec::new(),
            })
            .map_err(dom_error)?;
        custom_elements::upgrade_created_element(ctx, &self.realm, id)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(name = "createAttribute", coerce)]
    fn create_attribute(&self, ctx: &mut Ctx, qualified_name: &str) -> OpResult<Value> {
        attributes::create_attribute(ctx, &self.realm, qualified_name)
    }

    #[method(name = "createAttributeNS", coerce)]
    fn create_attribute_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        qualified_name: &str,
    ) -> OpResult<Value> {
        attributes::create_attribute_ns(ctx, &self.realm, namespace_uri, qualified_name)
    }

    #[method(coerce)]
    fn create_text_node(&self, ctx: &mut Ctx, text: &str) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Text(text.to_owned()))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(name = "createCDATASection", coerce)]
    fn create_cdata_section(&self, ctx: &mut Ctx, data: &str) -> OpResult<Value> {
        if self.realm.is_html_document {
            return Err(OpError::new(
                "NotSupportedError",
                "CDATA sections cannot be created in an HTML document",
            ));
        }
        if data.contains("]]>") {
            return Err(OpError::new(
                "InvalidCharacterError",
                "CDATA data cannot contain ]]>",
            ));
        }
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::CData(data.to_owned()))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn create_comment(&self, ctx: &mut Ctx, text: &str) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Comment(text.to_owned()))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn create_event(&self, ctx: &mut Ctx, interface: &str) -> OpResult<Value> {
        error_reporting::create_legacy_event(ctx, interface)
    }

    #[method(coerce)]
    fn create_processing_instruction(
        &self,
        ctx: &mut Ctx,
        target: &str,
        data: &str,
    ) -> OpResult<Value> {
        if !lumen_html::xml::is_xml_name(target) {
            return Err(OpError::new(
                "InvalidCharacterError",
                "processing instruction target is not a valid XML Name",
            ));
        }
        if data.contains("?>") {
            return Err(OpError::new(
                "InvalidCharacterError",
                "processing instruction data cannot contain ?>",
            ));
        }
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::ProcessingInstruction {
                target: target.into(),
                data: data.into(),
            })
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn get_element_by_id(&self, ctx: &mut Ctx, id: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let found = element_by_id(session.document(), id).map_err(dom_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, found))
    }

    fn create_document_fragment(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::DocumentFragment)
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn query_selector(&self, ctx: &mut Ctx, selector: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::query_selector(session.document(), session.document().root(), selector)
            .map_err(|error| selector_error(ctx, error))?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }
}

#[lumen_bind::class(name = "Node", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomNode {
    base: DomEventTarget,
    realm: Rc<DomRealm>,
    id: NodeId,
    collections: RefCell<HashMap<String, WeakValue>>,
}

impl lumen::embed::NativeIdentityOwner for DomNode {
    const TRACES_NATIVE_VALUES: bool = true;

    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.base.trace_callback_values(visit);
    }

    fn trace_native_identities(&self, epoch: u64, visit: &mut dyn FnMut(&Value)) {
        let realm = &self.realm;
        if self.id == realm.session.borrow().document().root() {
            // Real native tasks retain detached nodes independently of their
            // wrappers. Trace those components from the live owner Document.
            for node in realm.retained_nodes.borrow().keys() {
                let root = {
                    let session = realm.session.borrow();
                    selector::native_identity_root(session.document(), *node).ok()
                };
                if let Some(root) = root {
                    realm.trace_native_identity_component(epoch, root, visit);
                }
            }
        }
        // Every cached wrapper in a completed component is recorded, so most
        // callbacks return here without an O(depth) parent climb.
        if realm.native_identity_seen(epoch, self.id) {
            return;
        }
        let (root, document_root) = {
            let session = realm.session.borrow();
            let document = session.document();
            let Ok(root) = selector::native_identity_root(document, self.id) else {
                return;
            };
            (root, document.root())
        };
        realm.trace_native_identity_component(epoch, root, visit);

        if self.id == document_root {
            // The document wrapper is not stored in the ordinary node wrapper
            // map, but shares the root NodeId for epoch deduplication.
            realm.mark_native_identity(epoch, self.id);
        } else {
            // Every live Node exposes ownerDocument. Trace the connected tree
            // before emitting this weak identity so the document callback can
            // take the constant-time seen-node path later in this collection.
            realm.trace_document_wrapper(epoch, visit);
        }
    }
}

impl DomNode {
    fn replace_text_content(
        ctx: &mut Ctx,
        realm: &Rc<DomRealm>,
        node: NodeId,
        value: &str,
    ) -> OpResult<()> {
        Self::replace_text_content_without_checkpoint(realm, node, value)?;
        realm.flush_script_activations(ctx)
    }

    pub(crate) fn replace_text_content_without_checkpoint(
        realm: &Rc<DomRealm>,
        node: NodeId,
        value: &str,
    ) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let (old, is_container) = {
            let session = realm.session.borrow();
            let document = session.document();
            let is_container = matches!(
                document.kind(node).map_err(dom_error)?,
                NodeKind::Element { .. } | NodeKind::DocumentFragment
            );
            let old = if is_container {
                children(document, node).map_err(dom_error)?
            } else {
                Vec::new()
            };
            (old, is_container)
        };
        let select_mutation = if is_container {
            capture_select_mutation(realm, node, &[], &old)?
        } else {
            SelectMutationSnapshot::default()
        };
        let result = {
            let mut session = realm.session.borrow_mut();
            let document = session.document_mut();
            match document.kind(node).map_err(dom_error)? {
                NodeKind::Text(_)
                | NodeKind::CData(_)
                | NodeKind::Comment(_)
                | NodeKind::ProcessingInstruction { .. } => {
                    document.replace_data(node, value).map_err(dom_error)
                }
                NodeKind::Element { .. } | NodeKind::DocumentFragment => {
                    let replacement = if value.is_empty() {
                        document.create(NodeKind::DocumentFragment)
                    } else {
                        document.create(NodeKind::Text(value.to_owned()))
                    }
                    .map_err(dom_error)?;
                    let result = document
                        .replace_children(node, replacement)
                        .map_err(dom_error);
                    if value.is_empty() {
                        document.destroy_subtree(replacement).map_err(dom_error)?;
                    }
                    result
                }
                _ => Ok(()),
            }
        };
        if result.is_ok() {
            apply_select_mutation(realm, select_mutation)?;
            realm.invalidate_textarea_ancestor(node);
            realm.reap_detached(old);
        }
        result
    }

    fn normalized_attribute_name<'a>(&self, name: &'a str) -> Cow<'a, str> {
        let html_element = self.realm.is_html_document
            && matches!(
                self.realm.session.borrow().document().kind(self.id),
                Ok(NodeKind::Element {
                    namespace: Namespace::Html,
                    ..
                })
            );
        if html_element && name.bytes().any(|byte| byte.is_ascii_uppercase()) {
            Cow::Owned(name.to_ascii_lowercase())
        } else {
            Cow::Borrowed(name)
        }
    }

    fn get_null_attribute(&self, name: &str) -> OpResult<Option<String>> {
        self.realm
            .session
            .borrow()
            .document()
            .get_attribute_ns(self.id, None, name)
            .map_err(dom_error)
    }

    fn has_null_attribute(&self, name: &str) -> OpResult<bool> {
        Ok(self.get_null_attribute(name)?.is_some())
    }

    fn descendant_collection(
        &self,
        ctx: &mut Ctx,
        owner: Value,
        key: String,
        filter: DescendantFilter,
    ) -> Value {
        if let Some(value) = self
            .collections
            .borrow()
            .get(&key)
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomHtmlCollection {
            base: DomNodeList::descendants(self.realm.clone(), self.id, filter, owner),
        });
        self.collections
            .borrow_mut()
            .insert(key, ctx.weak_value(&value).expect("collection object"));
        value
    }

    fn insert_sibling_values(
        &self,
        ctx: &mut Ctx,
        values: Vec<Value>,
        after: bool,
    ) -> OpResult<()> {
        let (nodes, generated) = converted_dom_nodes(ctx, &self.realm, values)?;
        let (parent, before) = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let parent = document.parent(self.id).map_err(dom_error)?;
            let before = match (parent, after) {
                (Some(_), true) => document.next_sibling(self.id).map_err(dom_error)?,
                (Some(_), false) => Some(self.id),
                (None, _) => None,
            };
            (parent, before)
        };
        let Some(parent) = parent else {
            discard_generated_dom_nodes(&self.realm, &generated);
            return Ok(());
        };
        let select_mutation = capture_select_mutation(&self.realm, parent, &nodes, &[])?;
        let result = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .insert_many_before(parent, &nodes, before)
            .map_err(dom_error);
        if result.is_err() {
            discard_generated_dom_nodes(&self.realm, &generated);
        }
        result?;
        apply_select_mutation(&self.realm, select_mutation)?;
        self.realm.invalidate_textarea_ancestor(parent);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }

    fn replace_with_values(&self, ctx: &mut Ctx, values: Vec<Value>) -> OpResult<()> {
        let (nodes, generated) = converted_dom_nodes(ctx, &self.realm, values)?;
        let (parent, before) = {
            let session = self.realm.session.borrow();
            let document = session.document();
            (
                document.parent(self.id).map_err(dom_error)?,
                document.next_sibling(self.id).map_err(dom_error)?,
            )
        };
        let Some(parent) = parent else {
            discard_generated_dom_nodes(&self.realm, &generated);
            return Ok(());
        };
        let select_mutation = capture_select_mutation(&self.realm, parent, &nodes, &[self.id])?;
        let result = {
            let mut session = self.realm.session.borrow_mut();
            let document = session.document_mut();
            if nodes.contains(&self.id) {
                document.insert_many_before(parent, &nodes, before)
            } else {
                document
                    .remove(self.id)
                    .and_then(|()| document.insert_many_before(parent, &nodes, before))
            }
            .map_err(dom_error)
        };
        if result.is_err() {
            discard_generated_dom_nodes(&self.realm, &generated);
        }
        result?;
        apply_select_mutation(&self.realm, select_mutation)?;
        self.realm.invalidate_textarea_ancestor(parent);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }

    fn outer_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        let document = session.document();
        if document.is_html_document() {
            html::outer_html(document, self.id).map_err(dom_error)
        } else {
            lumen_html::xml::outer_html(document, self.id).map_err(|error| {
                if error == Error::WrongKind {
                    OpError::new(
                        "InvalidStateError",
                        "XML outerHTML serialization would not be well-formed",
                    )
                } else {
                    dom_error(error)
                }
            })
        }
    }

    fn insert_adjacent_markup(&self, ctx: &mut Ctx, position: &str, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let (parent, before) = adjacent_dom_insertion_point(document, self.id, position)?
            .ok_or_else(|| {
                OpError::new(
                    "NoModificationAllowedError",
                    "adjacent insertion requires a parent",
                )
            })?;
        if matches!(
            document.kind(parent).map_err(dom_error)?,
            NodeKind::Document
        ) {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "adjacent insertion cannot target a document",
            ));
        }
        let needs_body = match document.kind(parent).map_err(dom_error)? {
            NodeKind::Element {
                namespace, name, ..
            } => {
                document.is_html_document()
                    && namespace == &Namespace::Html
                    && lumen_html::xml::split_qname(name.as_str())
                        .map_or(name.as_str(), |(_, local)| local)
                        == "html"
            }
            _ => true,
        };
        let temporary_context = if needs_body {
            Some(
                document
                    .create(NodeKind::Element {
                        namespace: Namespace::Html,
                        name: "body".into(),
                        attributes: Vec::new(),
                    })
                    .map_err(dom_error)?,
            )
        } else {
            None
        };
        let parsed =
            parse_dom_markup_fragment(document, temporary_context.unwrap_or(parent), value);
        if let Some(context) = temporary_context {
            document.destroy_subtree(context).map_err(dom_error)?;
        }
        let fragment = parsed?;
        let inserted = children(document, fragment).map_err(dom_error)?;
        let inert_scripts = script_loading::scripts_in_subtree(document, fragment);
        drop(session);
        let select_mutation = match capture_select_mutation(&self.realm, parent, &inserted, &[]) {
            Ok(mutation) => mutation,
            Err(error) => {
                self.realm
                    .session
                    .borrow_mut()
                    .document_mut()
                    .destroy_subtree(fragment)
                    .map_err(dom_error)?;
                return Err(error);
            }
        };
        let result = {
            let mut session = self.realm.session.borrow_mut();
            let document = session.document_mut();
            let result = document
                .insert_before(parent, fragment, before)
                .map_err(dom_error);
            document.destroy_subtree(fragment).map_err(dom_error)?;
            result
        };
        result?;
        apply_select_mutation(&self.realm, select_mutation)?;
        for script in inert_scripts {
            self.realm.scripts.borrow_mut().mark_started(script);
        }
        self.realm.invalidate_textarea_ancestor(parent);
        for node in inserted {
            event_content_handlers::initialize_subtree(ctx, &self.realm, node)?;
            custom_elements::upgrade_created_element(ctx, &self.realm, node)?;
        }
        self.realm.flush_script_activations(ctx)
    }

    fn replace_outer_html(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let Some(parent) = document.parent(self.id).map_err(dom_error)? else {
            // A detached element has no parent whose children can be replaced.
            return Ok(());
        };
        if matches!(
            document.kind(parent).map_err(dom_error)?,
            NodeKind::Document
        ) {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "outerHTML cannot replace a document element",
            ));
        }
        let fragment = if document.is_html_document() {
            // DOM Parsing uses the parent as the fragment context. A fragment
            // parent has no element context, so use a temporary body element.
            if matches!(
                document.kind(parent).map_err(dom_error)?,
                NodeKind::DocumentFragment
            ) {
                let context = document
                    .create(NodeKind::Element {
                        namespace: Namespace::Html,
                        name: "body".into(),
                        attributes: Vec::new(),
                    })
                    .map_err(dom_error)?;
                let parsed = parse_dom_markup_fragment(document, context, value);
                document.destroy_subtree(context).map_err(dom_error)?;
                parsed?
            } else {
                parse_dom_markup_fragment(document, parent, value)?
            }
        } else {
            parse_dom_markup_fragment(document, parent, value)?
        };
        let inserted = children(document, fragment).map_err(dom_error)?;
        let inert_scripts = script_loading::scripts_in_subtree(document, fragment);
        for node in inert_scripts {
            self.realm.scripts.borrow_mut().mark_started(node);
        }
        drop(session);
        let select_mutation = match capture_select_mutation(
            &self.realm,
            parent,
            &inserted,
            &[self.id],
        ) {
            Ok(mutation) => mutation,
            Err(error) => {
                self.realm
                    .session
                    .borrow_mut()
                    .document_mut()
                    .destroy_subtree(fragment)
                    .map_err(dom_error)?;
                return Err(error);
            }
        };
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let result = document.replace(self.id, fragment).map_err(dom_error);
        // replace drains the fragment on success. On failure it remains a
        // detached parsed subtree, so reclaim it in either case.
        document.destroy_subtree(fragment).map_err(dom_error)?;
        drop(session);

        if result.is_ok() {
            apply_select_mutation(&self.realm, select_mutation)?;
            self.realm.invalidate_textarea_ancestor(parent);
            self.realm.reap_detached([self.id]);
            for node in inserted {
                event_content_handlers::initialize_subtree(ctx, &self.realm, node)?;
                custom_elements::upgrade_created_element(ctx, &self.realm, node)?;
            }
        }
        result?;
        self.realm.flush_script_activations(ctx)
    }
}

#[lumen_bind::methods]
impl DomNode {
    #[getter(rename(js = "baseURI"))]
    fn base_uri(&self) -> String {
        self.realm.base_url()
    }
    fn has_child_nodes(&self) -> OpResult<bool> {
        Ok(self
            .realm
            .session
            .borrow()
            .document()
            .first_child(self.id)
            .map_err(dom_error)?
            .is_some())
    }
    fn before(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        self.insert_sibling_values(ctx, nodes, false)
    }
    fn after(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        self.insert_sibling_values(ctx, nodes, true)
    }
    #[method(name = "replaceWith")]
    fn replace_with(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        self.replace_with_values(ctx, nodes)
    }
    fn get_root_node(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        let composed = match options {
            Some(value) => matches!(
                ctx.get_member(&value, "composed")
                    .map_err(|_| OpError::new("TypeError", "root options getter failed"))?,
                Value::Bool(true)
            ),
            None => false,
        };
        let root = self
            .realm
            .session
            .borrow()
            .document()
            .root_node(self.id, composed)
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, root))
    }
    #[getter]
    fn assigned_slot(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let slot = {
            let session = self.realm.session.borrow();
            let document = session.document();
            document
                .assigned_slot(self.id)
                .map_err(dom_error)?
                .filter(|slot| {
                    document
                        .root_node(*slot, false)
                        .ok()
                        .and_then(|root| document.shadow_mode(root).ok().flatten())
                        == Some(lumen_html::ShadowMode::Open)
                })
        };
        Ok(self.realm.wrap_option(ctx, slot))
    }
    fn append(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        let (ids, generated) = converted_dom_nodes(ctx, &self.realm, nodes)?;
        let select_mutation = capture_select_mutation(&self.realm, self.id, &ids, &[])?;
        let result = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .append_many(self.id, &ids)
            .map_err(dom_error);
        if result.is_err() {
            discard_generated_dom_nodes(&self.realm, &generated);
        }
        result?;
        apply_select_mutation(&self.realm, select_mutation)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }
    fn prepend(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        let (ids, generated) = converted_dom_nodes(ctx, &self.realm, nodes)?;
        let before = self
            .realm
            .session
            .borrow()
            .document()
            .first_child(self.id)
            .map_err(dom_error)?;
        let select_mutation = capture_select_mutation(&self.realm, self.id, &ids, &[])?;
        let result = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .insert_many_before(self.id, &ids, before)
            .map_err(dom_error);
        if result.is_err() {
            discard_generated_dom_nodes(&self.realm, &generated);
        }
        result?;
        apply_select_mutation(&self.realm, select_mutation)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }
    fn replace_children(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        let (ids, generated) = converted_dom_nodes(ctx, &self.realm, nodes)?;
        let mut session = self.realm.session.borrow_mut();
        let old = children(session.document(), self.id).map_err(dom_error)?;
        drop(session);
        let select_mutation = capture_select_mutation(&self.realm, self.id, &ids, &old)?;
        let mut session = self.realm.session.borrow_mut();
        let result = session
            .document_mut()
            .replace_children_many(self.id, &ids)
            .map_err(dom_error);
        if result.is_err() {
            for id in generated {
                let _ = session.document_mut().destroy_subtree(id);
            }
        }
        result?;
        drop(session);
        apply_select_mutation(&self.realm, select_mutation)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.reap_detached(old);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }
    #[getter(name = "namespaceURI")]
    fn namespace_uri(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element {
                namespace: Namespace::Other(value),
                ..
            } if value.is_empty() => None,
            NodeKind::Element { namespace, .. } => Some(
                match namespace {
                    Namespace::Html => "http://www.w3.org/1999/xhtml",
                    Namespace::Svg => "http://www.w3.org/2000/svg",
                    Namespace::MathMl => "http://www.w3.org/1998/Math/MathML",
                    Namespace::Other(value) => value,
                }
                .into(),
            ),
            _ => None,
        })
    }
    #[getter]
    fn local_name(&self) -> OpResult<Option<String>> {
        Ok(
            match self
                .realm
                .session
                .borrow()
                .document()
                .kind(self.id)
                .map_err(dom_error)?
            {
                NodeKind::Element { name, .. } => Some(String::from(
                    name.as_str()
                        .rsplit_once(':')
                        .map_or(name.as_str(), |(_, local)| local),
                )),
                _ => None,
            },
        )
    }
    #[getter]
    fn prefix(&self) -> OpResult<Option<String>> {
        Ok(
            match self
                .realm
                .session
                .borrow()
                .document()
                .kind(self.id)
                .map_err(dom_error)?
            {
                NodeKind::Element { name, .. } => name
                    .as_str()
                    .split_once(':')
                    .map(|(prefix, _)| prefix.to_owned()),
                _ => None,
            },
        )
    }
    #[getter]
    fn tag_name(&self) -> OpResult<String> {
        self.node_name()
    }
    #[getter]
    fn is_connected(&self) -> OpResult<bool> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let mut current = Some(self.id);
        while let Some(node) = current {
            if node == document.root() {
                return Ok(true);
            }
            current = document.shadow_including_parent(node).map_err(dom_error)?;
        }
        Ok(false)
    }
    fn contains(&self, other: Option<&DomNode>) -> OpResult<bool> {
        let Some(other) = other else {
            return Ok(false);
        };
        if !Rc::ptr_eq(&self.realm, &other.realm) {
            return Ok(false);
        }
        let session = self.realm.session.borrow();
        let mut current = Some(other.id);
        while let Some(node) = current {
            if node == self.id {
                return Ok(true);
            }
            current = session.document().parent(node).map_err(dom_error)?;
        }
        Ok(false)
    }
    #[method(coerce)]
    fn has_attribute(&self, name: &str) -> OpResult<bool> {
        Ok(self.get_attribute(name)?.is_some())
    }
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .collections
            .borrow()
            .get("style")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomStyle {
            realm: self.realm.clone(),
            node: self.id,
            computed: false,
            _owner: this.0,
        });
        self.collections.borrow_mut().insert(
            "style".into(),
            ctx.weak_value(&value).expect("style object"),
        );
        value
    }

    #[setter(coerce)]
    fn set_style(&self, value: &str) -> OpResult<()> {
        let serialized = lumen_html::css::cssom_declaration_text(value);
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.id, None, "style", &serialized)
            .map_err(dom_error)
    }

    #[method(coerce)]
    fn query_selector_all(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        query: &str,
    ) -> OpResult<DomNodeList> {
        let nodes =
            selector::query_selector_all(self.realm.session.borrow().document(), self.id, query)
                .map_err(|error| selector_error(ctx, error))?;
        Ok(DomNodeList::snapshot(self.realm.clone(), nodes, this.0))
    }

    #[getter]
    fn child_nodes(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .collections
            .borrow()
            .get("childNodes")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomNodeList::children(
            self.realm.clone(),
            self.id,
            false,
            this.0,
        ));
        self.collections.borrow_mut().insert(
            "childNodes".into(),
            ctx.weak_value(&value).expect("collection object"),
        );
        value
    }
    #[getter]
    fn children(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .collections
            .borrow()
            .get("children")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomHtmlCollection {
            base: DomNodeList::children(self.realm.clone(), self.id, true, this.0),
        });
        self.collections.borrow_mut().insert(
            "children".into(),
            ctx.weak_value(&value).expect("collection object"),
        );
        value
    }
    #[getter]
    fn class_list(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self
            .collections
            .borrow()
            .get("classList")
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let value = ctx.new_instance(DomTokenList {
            realm: self.realm.clone(),
            node: self.id,
            owner: this.0,
        });
        self.collections.borrow_mut().insert(
            "classList".into(),
            ctx.weak_value(&value).expect("collection object"),
        );
        value
    }

    #[getter]
    fn owner_document(&self, ctx: &mut Ctx) -> Value {
        let root = self.realm.session.borrow().document().root();
        if self.id == root {
            Value::Null
        } else {
            self.realm.wrap(ctx, root)
        }
    }

    #[getter]
    fn node_value(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Text(value) | NodeKind::Comment(value) => Some(value.clone()),
            NodeKind::CData(value) => Some(value.clone()),
            NodeKind::ProcessingInstruction { data, .. } => Some(data.clone()),
            _ => None,
        })
    }

    #[setter(coerce)]
    fn set_node_value(&self, value: Option<&str>) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let is_text = matches!(
            doc.kind(self.id).map_err(dom_error)?,
            NodeKind::Text(_) | NodeKind::CData(_)
        );
        let result = match doc.kind(self.id).map_err(dom_error)? {
            NodeKind::Text(_)
            | NodeKind::CData(_)
            | NodeKind::Comment(_)
            | NodeKind::ProcessingInstruction { .. } => doc
                .replace_data(self.id, value.unwrap_or(""))
                .map_err(dom_error),
            _ => Ok(()),
        };
        drop(session);
        if result.is_ok() && is_text {
            self.realm.invalidate_textarea_ancestor(self.id);
        }
        result
    }

    #[getter]
    fn node_type(&self) -> OpResult<u8> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element { .. } => 1,
            NodeKind::Attribute { .. } => 2,
            NodeKind::Text(_) => 3,
            NodeKind::CData(_) => 4,
            NodeKind::ProcessingInstruction { .. } => 7,
            NodeKind::Comment(_) => 8,
            NodeKind::Document => 9,
            NodeKind::DocumentType(_) => 10,
            NodeKind::DocumentFragment => 11,
        })
    }

    #[getter]
    fn node_name(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            } if self.realm.is_html_document => name.to_ascii_uppercase(),
            NodeKind::Element { name, .. } => String::from(name.as_str()),
            NodeKind::Attribute { qualified_name, .. } => String::from(qualified_name.as_str()),
            NodeKind::Text(_) => "#text".into(),
            NodeKind::CData(_) => "#cdata-section".into(),
            NodeKind::Comment(_) => "#comment".into(),
            NodeKind::Document => "#document".into(),
            NodeKind::DocumentFragment => "#document-fragment".into(),
            NodeKind::DocumentType(name) => name.clone(),
            NodeKind::ProcessingInstruction { target, .. } => target.clone(),
        })
    }

    #[getter]
    fn parent_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let parent = self
            .realm
            .session
            .borrow()
            .document()
            .parent(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, parent))
    }

    #[getter]
    fn parent_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let parent = {
            let session = self.realm.session.borrow();
            let document = session.document();
            document
                .parent(self.id)
                .map_err(dom_error)?
                .filter(|id| matches!(document.kind(*id), Ok(NodeKind::Element { .. })))
        };
        Ok(self.realm.wrap_option(ctx, parent))
    }

    #[getter]
    fn first_element_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let mut child = document.first_child(self.id).map_err(dom_error)?;
            while let Some(id) = child {
                if matches!(
                    document.kind(id).map_err(dom_error)?,
                    NodeKind::Element { .. }
                ) {
                    break;
                }
                child = document.next_sibling(id).map_err(dom_error)?;
            }
            child
        };
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn last_element_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let mut child = document.last_child(self.id).map_err(dom_error)?;
            while let Some(id) = child {
                if matches!(
                    document.kind(id).map_err(dom_error)?,
                    NodeKind::Element { .. }
                ) {
                    break;
                }
                child = document.previous_sibling(id).map_err(dom_error)?;
            }
            child
        };
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn child_element_count(&self) -> OpResult<u32> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let mut child = document.first_child(self.id).map_err(dom_error)?;
        let mut count = 0;
        while let Some(id) = child {
            if matches!(
                document.kind(id).map_err(dom_error)?,
                NodeKind::Element { .. }
            ) {
                count += 1;
            }
            child = document.next_sibling(id).map_err(dom_error)?;
        }
        Ok(count)
    }

    #[getter]
    fn first_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = self
            .realm
            .session
            .borrow()
            .document()
            .first_child(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn last_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = self
            .realm
            .session
            .borrow()
            .document()
            .last_child(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn previous_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = self
            .realm
            .session
            .borrow()
            .document()
            .previous_sibling(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    #[getter]
    fn next_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = self
            .realm
            .session
            .borrow()
            .document()
            .next_sibling(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    #[getter]
    fn previous_element_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let mut sibling = document.previous_sibling(self.id).map_err(dom_error)?;
            while let Some(id) = sibling {
                if matches!(
                    document.kind(id).map_err(dom_error)?,
                    NodeKind::Element { .. }
                ) {
                    break;
                }
                sibling = document.previous_sibling(id).map_err(dom_error)?;
            }
            sibling
        };
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    #[getter]
    fn next_element_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let mut sibling = document.next_sibling(self.id).map_err(dom_error)?;
            while let Some(id) = sibling {
                if matches!(
                    document.kind(id).map_err(dom_error)?,
                    NodeKind::Element { .. }
                ) {
                    break;
                }
                sibling = document.next_sibling(id).map_err(dom_error)?;
            }
            sibling
        };
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    fn append_child(&self, ctx: &mut Ctx, child: Value) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        insert_dom_node(ctx, &self.realm, self.id, child, Value::Null)
    }

    fn replace_child(&self, ctx: &mut Ctx, new_child: Value, old_child: Value) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        // Extract only native identity here. Cross-document replacement can
        // rebind the original wrapper in place during adoption, so no typed
        // projection may stay borrowed across that operation.
        let (new_realm, new_id) =
            ctx.with_instance::<DomNode, _>(&new_child, |node| (node.realm.clone(), node.id))?;
        let (old_realm, old_id) =
            ctx.with_instance::<DomNode, _>(&old_child, |node| (node.realm.clone(), node.id))?;
        if !Rc::ptr_eq(&old_realm, &self.realm)
            || self
                .realm
                .session
                .borrow()
                .document()
                .parent(old_id)
                .map_err(dom_error)?
                != Some(self.id)
        {
            return Err(OpError::new("NotFoundError", "node is not a child"));
        }
        if Rc::ptr_eq(&self.realm, &new_realm) && old_id == new_id {
            return Ok(self.realm.wrap(ctx, old_id));
        }

        let (new_has_options, source_select) = {
            let session = new_realm.session.borrow();
            let document = session.document();
            let has_options =
                lumen_html::forms::subtree_contains_select_option(document, new_id)
                    .map_err(dom_error)?;
            let source_select = if has_options {
                document
                    .parent(new_id)
                    .map_err(dom_error)?
                    .map(|parent| lumen_html::forms::select_ancestor(document, parent))
                    .transpose()
                    .map_err(dom_error)?
                    .flatten()
            } else {
                None
            };
            (has_options, source_select)
        };
        let target_select = {
            let session = self.realm.session.borrow();
            let document = session.document();
            let old_has_options =
                lumen_html::forms::subtree_contains_select_option(document, old_id)
                    .map_err(dom_error)?;
            if old_has_options || new_has_options {
                lumen_html::forms::select_ancestor(document, self.id).map_err(dom_error)?
            } else {
                None
            }
        };

        // Validate the complete replacement against both source and target
        // documents before adoption detaches anything from the source tree.
        if Rc::ptr_eq(&self.realm, &new_realm) {
            let target = self.realm.session.borrow();
            target
                .document()
                .validate_replace_from(target.document(), old_id, new_id)
                .map_err(dom_error)?;
        } else {
            let target = self.realm.session.borrow();
            let source = new_realm.session.borrow();
            target
                .document()
                .validate_replace_from(source.document(), old_id, new_id)
                .map_err(dom_error)?;
        }

        let replacement_id = if Rc::ptr_eq(&self.realm, &new_realm) {
            new_id
        } else {
            DomRealm::adopt_node_from(&self.realm, ctx, &new_realm, new_id)?
        };
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .replace(old_id, replacement_id)
            .map_err(dom_error)?;
        if let Some(select) = source_select {
            if !Rc::ptr_eq(&self.realm, &new_realm)
                || target_select != Some(select)
            {
                forms::select_option_list_changed(&new_realm, select)?;
            }
        }
        if let Some(select) = target_select {
            let preferred = if new_has_options {
                forms::selected_option_in_subtree(&self.realm, replacement_id)?
            } else {
                None
            };
            forms::select_option_list_changed_with_preferred(
                &self.realm,
                select,
                preferred,
            )?;
        }
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        let removed = self.realm.wrap(ctx, old_id);
        self.realm.reap_detached([old_id]);
        Ok(removed)
    }

    fn insert_before(&self, ctx: &mut Ctx, child: Value, before: Value) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        insert_dom_node(ctx, &self.realm, self.id, child, before)
    }

    fn remove_child(&self, ctx: &mut Ctx, child: &DomNode) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        if !Rc::ptr_eq(&self.realm, &child.realm)
            || self
                .realm
                .session
                .borrow()
                .document()
                .parent(child.id)
                .map_err(dom_error)?
                != Some(self.id)
        {
            return Err(OpError::new("NotFoundError", "node is not a child"));
        }
        let affected_select = select_for_removed_option_subtree(
            &self.realm,
            child.id,
            Some(self.id),
        )?;
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(child.id)
            .map_err(dom_error)?;
        if let Some(select) = affected_select {
            forms::select_option_list_changed(&self.realm, select)?;
        }
        self.realm.invalidate_textarea_ancestor(self.id);
        let value = self.realm.wrap(ctx, child.id);
        self.realm.reap_detached([child.id]);
        Ok(value)
    }

    fn remove(&self) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let parent = self
            .realm
            .session
            .borrow()
            .document()
            .parent(self.id)
            .map_err(dom_error)?;
        let affected_select =
            select_for_removed_option_subtree(&self.realm, self.id, parent)?;
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(self.id)
            .map_err(dom_error)?;
        if let Some(select) = affected_select {
            forms::select_option_list_changed(&self.realm, select)?;
        }
        if let Some(parent) = parent {
            self.realm.invalidate_textarea_ancestor(parent);
        }
        self.realm.reap_detached([self.id]);
        Ok(())
    }

    #[method(coerce)]
    fn clone_node(&self, ctx: &mut Ctx, deep: Option<bool>) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let deep = deep.unwrap_or(false);
        let is_document = {
            let session = self.realm.session.borrow();
            self.id == session.document().root()
        };
        if is_document {
            let document = self
                .realm
                .session
                .borrow()
                .document()
                .clone_document(deep)
                .map_err(dom_error)?;
            let clone_realm = DomRealm::realm_from_document_with_interface(
                document,
                &self.realm.content_type,
                self.realm.is_html_document,
                false,
                self.realm.document_interface,
            );
            clone_realm.set_about_base_url(self.realm.about_base_url.borrow().clone());
            if let Some(url) = self.realm.document_url() {
                clone_realm.set_document_url(url);
            }
            if let Some(origin) = self.realm.document_origin() {
                clone_realm.set_document_origin(origin);
            }
            let clone_root = clone_realm.session.borrow().document().root();
            let script_pairs = {
                let source = self.realm.session.borrow();
                let copy = clone_realm.session.borrow();
                script_loading::paired_subtree_nodes(
                    source.document(),
                    copy.document(),
                    self.id,
                    clone_root,
                )
            };
            {
                let session = clone_realm.session.borrow();
                let source_form_state = self.realm.forms.borrow();
                let mut target_form_state = clone_realm.forms.borrow_mut();
                forms::clone_live_values_into(
                    &source_form_state,
                    &mut target_form_state,
                    session.document(),
                    &script_pairs,
                );
            }
            {
                let source_scripts = self.realm.scripts.borrow();
                clone_realm
                    .scripts
                    .borrow_mut()
                    .clone_states_from(&source_scripts, &script_pairs);
            }
            event_content_handlers::initialize_subtree(ctx, &clone_realm, clone_root)?;
            return Ok(clone_realm.document_value(ctx));
        }

        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let id = doc.clone_node(self.id, deep).map_err(dom_error)?;
        let script_pairs = script_loading::paired_subtree_nodes(
            session.document(),
            session.document(),
            self.id,
            id,
        );
        let form_state = self.realm.forms.borrow().clone();
        forms::clone_live_values_into(
            &form_state,
            &mut self.realm.forms.borrow_mut(),
            session.document(),
            &script_pairs,
        );
        drop(session);
        self.realm.scripts.borrow_mut().clone_states(&script_pairs);
        event_content_handlers::initialize_subtree(ctx, &self.realm, id)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn set_attribute(&self, ctx: &mut Ctx, name: &str, value: &str) -> OpResult<()> {
        let name = self.normalized_attribute_name(name);
        if !lumen_html::xml::is_valid_attribute_local_name(name.as_ref()) {
            return Err(error_reporting::dom_exception(
                ctx,
                "InvalidCharacterError",
                "attribute name is not a valid attribute local name",
            ));
        }
        let namespace_uri = self
            .realm
            .session
            .borrow()
            .document()
            .attribute_namespace_uri(self.id, name.as_ref())
            .map_err(dom_error)?;
        self.set_attribute_by_name_core(name.as_ref(), value)?;
        event_content_handlers::attribute_changed(
            ctx,
            &self.realm,
            self.id,
            namespace_uri.as_deref(),
            name.as_ref(),
            Some(value),
        )?;
        self.realm.flush_script_activations(ctx)
    }

    /// `setAttribute` matches the first existing qualified name and preserves its namespace.
    fn set_attribute_by_name_core(&self, name: &str, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let namespaced = self
            .realm
            .session
            .borrow()
            .document()
            .attribute_namespace_uri(self.id, name)
            .map_err(dom_error)?
            .is_some();
        if !namespaced {
            forms::prepare_input_attribute_change(&self.realm, self.id, name)?;
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(self.id, name, value)
            .map_err(dom_error)?;
        if !namespaced {
            forms::resanitize_input_after_attribute_change(&self.realm, self.id, name)?;
        }
        self.after_attribute_write(name, namespaced)
    }

    fn set_attribute_core(&self, name: &str, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        forms::prepare_input_attribute_change(&self.realm, self.id, name)?;
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.id, None, name, value)
            .map_err(dom_error)?;
        forms::resanitize_input_after_attribute_change(&self.realm, self.id, name)?;
        self.after_attribute_write(name, false)
    }

    fn after_attribute_write(&self, name: &str, namespaced: bool) -> OpResult<()> {
        if !namespaced && matches!(name, "width" | "height") {
            self.realm.sync_canvas()?;
        }
        if !namespaced && name == "value" {
            let control = matches!(
                self.realm.session.borrow().document().kind(self.id),
                Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "input" | "textarea")
            );
            if control {
                self.realm.invalidate_editing_for_value_change(self.id);
            }
        }
        self.realm.sync_image_bitmaps()?;
        Ok(())
    }

    #[method(coerce)]
    fn get_attribute(&self, name: &str) -> OpResult<Option<String>> {
        let name = self.normalized_attribute_name(name);
        let session = self.realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(self.id).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "attributes require an element"));
        };
        Ok(attributes
            .iter()
            .find(|(key, _)| key.as_str() == name.as_ref())
            .map(|(_, value)| value.clone()))
    }

    #[method(name = "getAttributeNS", coerce)]
    fn get_attribute_ns(
        &self,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<Option<String>> {
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        self.realm
            .session
            .borrow()
            .document()
            .get_attribute_ns(self.id, namespace_uri, local_name)
            .map_err(dom_error)
    }

    #[method(name = "hasAttributeNS", coerce)]
    fn has_attribute_ns(&self, namespace_uri: Option<&str>, local_name: &str) -> OpResult<bool> {
        Ok(self.get_attribute_ns(namespace_uri, local_name)?.is_some())
    }

    #[method(name = "setAttributeNS", coerce)]
    fn set_attribute_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        qualified_name: &str,
        value: &str,
    ) -> OpResult<()> {
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        namespace_for_qname(ctx, namespace_uri, qualified_name)?;
        let _html_allocations = enter_html_allocation_category();
        if namespace_uri.is_none() {
            forms::prepare_input_attribute_change(&self.realm, self.id, qualified_name)?;
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.id, namespace_uri, qualified_name, value)
            .map_err(dom_error)?;
        if namespace_uri.is_none() {
            forms::resanitize_input_after_attribute_change(&self.realm, self.id, qualified_name)?;
        }
        self.after_attribute_write(qualified_name, namespace_uri.is_some())?;
        event_content_handlers::attribute_changed(
            ctx,
            &self.realm,
            self.id,
            namespace_uri,
            qualified_name,
            Some(value),
        )?;
        self.realm.flush_script_activations(ctx)
    }

    #[method(name = "removeAttributeNS", coerce)]
    fn remove_attribute_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<()> {
        let namespace_uri = namespace_uri.filter(|uri| !uri.is_empty());
        let _html_allocations = enter_html_allocation_category();
        let materialized = {
            let session = self.realm.session.borrow();
            let document = session.document();
            materialized_attribute_by_ns(document, self.id, namespace_uri, local_name)
        };
        let existed = self
            .realm
            .session
            .borrow()
            .document()
            .get_attribute_ns(self.id, namespace_uri, local_name)
            .map_err(dom_error)?
            .is_some();
        if namespace_uri.is_none() && existed {
            forms::prepare_input_attribute_change(&self.realm, self.id, local_name)?;
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute_ns(self.id, namespace_uri, local_name)
            .map_err(dom_error)?;
        if namespace_uri.is_none() {
            forms::resanitize_input_after_attribute_change(&self.realm, self.id, local_name)?;
        }
        if let Some(attribute) = materialized {
            self.realm.reap_detached([attribute]);
        }
        self.after_attribute_removal(local_name, namespace_uri.is_some())?;
        if existed {
            event_content_handlers::attribute_changed(
                ctx,
                &self.realm,
                self.id,
                namespace_uri,
                local_name,
                None,
            )?;
        }
        self.realm.flush_script_activations(ctx)
    }

    #[getter]
    fn id(&self) -> OpResult<String> {
        Ok(self.get_null_attribute("id")?.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_id(&self, value: &str) -> OpResult<()> {
        self.set_attribute_core("id", value)
    }

    #[getter]
    fn class_name(&self) -> OpResult<String> {
        Ok(self.get_null_attribute("class")?.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_class_name(&self, value: &str) -> OpResult<()> {
        self.set_attribute_core("class", value)
    }

    #[method(coerce)]
    fn remove_attribute(&self, ctx: &mut Ctx, name: &str) -> OpResult<()> {
        let name = self.normalized_attribute_name(name);
        let existed = self.get_attribute(name.as_ref())?.is_some();
        let namespace_uri = self
            .realm
            .session
            .borrow()
            .document()
            .attribute_namespace_uri(self.id, name.as_ref())
            .map_err(dom_error)?;
        self.remove_attribute_by_name_core(name.as_ref())?;
        if existed {
            event_content_handlers::attribute_changed(
                ctx,
                &self.realm,
                self.id,
                namespace_uri.as_deref(),
                name.as_ref(),
                None,
            )?;
        }
        self.realm.flush_script_activations(ctx)
    }

    fn remove_attribute_by_name_core(&self, name: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let namespaced = self
            .realm
            .session
            .borrow()
            .document()
            .attribute_namespace_uri(self.id, name)
            .map_err(dom_error)?
            .is_some();
        if !namespaced {
            forms::prepare_input_attribute_change(&self.realm, self.id, name)?;
        }
        let materialized = {
            let session = self.realm.session.borrow();
            materialized_attribute_by_name(session.document(), self.id, name)
        };
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(self.id, name)
            .map_err(dom_error)?;
        if !namespaced {
            forms::resanitize_input_after_attribute_change(&self.realm, self.id, name)?;
        }
        if let Some(attribute) = materialized {
            self.realm.reap_detached([attribute]);
        }
        self.after_attribute_removal(name, namespaced)
    }

    fn remove_attribute_core(&self, name: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        forms::prepare_input_attribute_change(&self.realm, self.id, name)?;
        let materialized = {
            let session = self.realm.session.borrow();
            materialized_attribute_by_ns(session.document(), self.id, None, name)
        };
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute_ns(self.id, None, name)
            .map_err(dom_error)?;
        forms::resanitize_input_after_attribute_change(&self.realm, self.id, name)?;
        if let Some(attribute) = materialized {
            self.realm.reap_detached([attribute]);
        }
        self.after_attribute_removal(name, false)
    }

    fn after_attribute_removal(&self, name: &str, namespaced: bool) -> OpResult<()> {
        if !namespaced && matches!(name, "width" | "height") {
            self.realm.sync_canvas()?;
        }
        if !namespaced && name == "value" {
            let control = matches!(
                self.realm.session.borrow().document().kind(self.id),
                Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "input" | "textarea")
            );
            if control {
                self.realm.invalidate_editing_for_value_change(self.id);
            }
        }
        self.realm.sync_image_bitmaps()?;
        Ok(())
    }

    #[getter(name = "textContent")]
    fn text_content(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Text(text) | NodeKind::CData(text) | NodeKind::Comment(text) => {
                return Ok(Some(text.clone()));
            }
            NodeKind::ProcessingInstruction { data, .. } => return Ok(Some(data.clone())),
            NodeKind::Document | NodeKind::DocumentType(_) => return Ok(None),
            _ => {}
        }
        let mut out = String::new();
        session
            .document()
            .append_descendant_text(self.id, &mut out)
            .map_err(dom_error)?;
        Ok(Some(out))
    }

    #[setter(name = "textContent", coerce)]
    fn set_text_content(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        Self::replace_text_content(ctx, &self.realm, self.id, value)
    }

    #[getter(name = "innerHTML")]
    fn inner_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        html::inner_html(session.document(), self.id).map_err(dom_error)
    }

    #[setter(name = "innerHTML", coerce)]
    fn set_inner_html(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let (fragment, target, inserted, old) = {
            let mut session = self.realm.session.borrow_mut();
            let document = session.document_mut();
            let context = document
                .shadow_host(self.id)
                .map_err(dom_error)?
                .unwrap_or(self.id);
            let fragment = parse_dom_markup_fragment(document, context, value)
                .map_err(|error| dom_markup_error(ctx, error))?;
            let inert_scripts = script_loading::scripts_in_subtree(document, fragment);
            for node in inert_scripts {
                self.realm.scripts.borrow_mut().mark_started(node);
            }
            let target = document
                .template_content(self.id)
                .map_err(dom_error)?
                .unwrap_or(self.id);
            let inserted = children(document, fragment).map_err(dom_error)?;
            let old = children(document, target).map_err(dom_error)?;
            (fragment, target, inserted, old)
        };
        let select_mutation = capture_select_mutation(&self.realm, target, &[fragment], &old)?;
        let result = {
            let mut session = self.realm.session.borrow_mut();
            let document = session.document_mut();
            let result = document
                .replace_children(target, fragment)
                .map_err(dom_error);
            document.destroy_subtree(fragment).map_err(dom_error)?;
            result
        };
        if result.is_ok() {
            apply_select_mutation(&self.realm, select_mutation)?;
            self.realm.invalidate_textarea_ancestor(self.id);
            self.realm.reap_detached(old);
            for node in inserted {
                event_content_handlers::initialize_subtree(ctx, &self.realm, node)?;
            }
        }
        result?;
        self.realm.flush_script_activations(ctx)
    }

    #[method(coerce)]
    fn query_selector(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::query_selector(session.document(), self.id, query)
            .map_err(|error| selector_error(ctx, error))?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }

    #[method(coerce)]
    fn matches(&self, ctx: &mut Ctx, query: &str) -> OpResult<bool> {
        let session = self.realm.session.borrow();
        selector::matches(session.document(), self.id, query)
            .map_err(|error| selector_error(ctx, error))
    }

    #[method(coerce)]
    fn closest(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::closest(session.document(), self.id, query)
            .map_err(|error| selector_error(ctx, error))?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }
}

impl DomRealm {
    fn selection_value(self: &Rc<Self>, ctx: &mut Ctx) -> Value {
        if let Some(value) = self
            .selection_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let data = self
            .selection
            .borrow_mut()
            .get_or_insert_with(|| range::SelectionData::new(self.ranges.clone()))
            .clone();
        let value = ctx.new_instance(range::DomSelection {
            realm: self.clone(),
            data,
        });
        *self.selection_wrapper.borrow_mut() = ctx.weak_value(&value);
        value
    }

    pub(crate) fn realm_from_document(document: lumen_html::Document) -> Rc<Self> {
        Self::realm_from_document_with_metadata(document, "text/html", true, false)
    }

    pub(crate) fn realm_from_document_with_metadata(
        document: lumen_html::Document,
        content_type: &str,
        is_html_document: bool,
        has_browsing_context: bool,
    ) -> Rc<Self> {
        Self::realm_from_document_with_interface(
            document,
            content_type,
            is_html_document,
            has_browsing_context,
            DocumentInterface::Document,
        )
    }

    fn realm_from_document_with_interface(
        mut document: lumen_html::Document,
        content_type: &str,
        is_html_document: bool,
        has_browsing_context: bool,
        document_interface: DocumentInterface,
    ) -> Rc<Self> {
        document.set_html_document(is_html_document);
        let forms = Rc::new(RefCell::new(forms::FormState::default()));
        let validity_state = Rc::downgrade(&forms);
        let validity_generation = Rc::downgrade(&forms);
        document.set_validity_resolver(
            Rc::new(move |document, node| {
                let state = validity_state.upgrade()?;
                let state = state.borrow();
                Some(lumen_html::forms::validity_with_view(
                    document, node, &*state,
                ))
            }),
            Rc::new(move || {
                validity_generation
                    .upgrade()
                    .map_or(0, |state| state.borrow().validity_generation())
            }),
        );
        let selector_state = Rc::downgrade(&forms);
        document.set_form_selector_state_resolver(Rc::new(move |document, node| {
            let state = selector_state.upgrade()?;
            let state = state.borrow();
            Some(lumen_html::forms::FormSelectorState {
                checkedness: lumen_html::forms::ValidityStateView::checkedness(&*state, node),
                selectedness: lumen_html::forms::ValidityStateView::selectedness(&*state, node),
                single_select_option: state.single_select_option(document, node),
                user_validity_interacted: Some(state.has_user_validity_interaction(node)),
                placeholder_shown: Some(lumen_html::forms::placeholder_shown(
                    document,
                    node,
                    lumen_html::forms::ValidityStateView::value_override(&*state, node),
                )),
                auto_value_directionality:
                    lumen_html::directionality::is_auto_directionality_form_associated(
                        document, node,
                    )
                    .then(|| {
                        lumen_html::forms::ValidityStateView::value_override(&*state, node)
                            .map(lumen_html::directionality::control_value_direction)
                    })
                    .flatten(),
            })
        }));
        if !has_browsing_context {
            // A cloned or otherwise detached document has no browsing
            // context, so its scripting mode is disabled even when the source
            // document was parsed for an active window.
            document.set_scripting_enabled(false);
        }
        // A document produced by DOMParser (or createHTMLDocument) starts
        // without a browsing context. Its parser-created scripts are inert,
        // including if a later adoption moves them into an active document.
        let inert_parser_scripts = if has_browsing_context {
            Vec::new()
        } else {
            let mut scripts = script_loading::parser_scripts(&document);
            scripts.extend(script_loading::template_scripts(&document));
            scripts
        };
        let realm = Rc::new(DomRealm {
            details_controller: RefCell::new(std::rc::Weak::new()),
            timeline_sample: Cell::new(lumen_host::perf::web_now_ms()),
            media_capture: media_capture::RealmMediaCapture::default(),
            browser_services: browser_services::RealmBrowserServices::default(),
            document_identity: Rc::new(DocumentIdentity::default()),
            about_base_url: RefCell::new(None),
            document_base_url: RefCell::new(DocumentBaseUrlCache::default()),
            document_interface,
            content_type: content_type.to_owned(),
            document_encoding: Cell::new("UTF-8"),
            is_html_document,
            has_browsing_context,
            file_picker_host: RefCell::new(None),
            form_submission_host: RefCell::new(None),
            canvases: canvas::CanvasRegistry::default(),
            forms,
            ranges: range::RangeRegistry::new(),
            iterators: document_utilities::IteratorRegistry::new(),
            tree_walkers: document_utilities::TreeWalkerRegistry::new(),
            selection: RefCell::new(None),
            selection_wrapper: RefCell::new(None),
            ready_state: Cell::new(if has_browsing_context {
                DocumentReadyState::Loading
            } else {
                DocumentReadyState::Complete
            }),
            current_script: Cell::new(None),
            document_parser: RefCell::new(None),
            parser_generation: Cell::new(0),
            scripts: RefCell::new(script_loading::ScriptLoader::default()),
            module_activations_enabled: Cell::new(false),
            script_capabilities: Rc::new(ScriptCapabilities::default()),
            classic_resource_activations_enabled: Cell::new(false),
            module_activations: RefCell::new(VecDeque::new()),
            dataset_intrinsics: RefCell::new(None),
            layout_flusher: RefCell::new(None),
            cookie_host: RefCell::new(None),
            font_loading: font_loading::FontLoading::default(),
            images: image_loading::ImageLoader::default(),
            media: RefCell::new(media::MediaController::default()),
            web_audio: RefCell::new(webaudio::WebAudioController::default()),
            image_request_roots: RefCell::new(HashMap::new()),
            mutation_sinks: RefCell::new(Vec::new()),
            session: Rc::new(RefCell::new(RenderSession::new(document))),
            wrappers: RefCell::new(HashMap::new()),
            identity_trace_epoch: Cell::new(0),
            identity_trace_nodes: RefCell::new(HashSet::new()),
            adopted_nodes: RefCell::new(HashMap::new()),
            document_wrapper: RefCell::new(None),
            implementation_wrapper: RefCell::new(None),
            detached: RefCell::new(Vec::new()),
            sweep_at: Cell::new(256),
            targets: RefCell::new(HashMap::new()),
            retained_nodes: RefCell::new(HashMap::new()),
            script_retentions: RefCell::new(HashMap::new()),
            window_target: RefCell::new(None),
            window_wrapper: RefCell::new(None),
            location_wrapper: RefCell::new(None),
            browsing_context: RefCell::new(std::rc::Weak::new()),
            frame_contexts: RefCell::new(HashMap::new()),
            pending_frame_contexts: RefCell::new(HashMap::new()),
            pending_frame_realms: RefCell::new(Vec::new()),
            pending_iframe_post_connections: RefCell::new(VecDeque::new()),
            focused: Cell::new(None),
            focus_visible: Cell::new(None),
            hover_target: Cell::new(None),
            active_targets: Cell::new([None, None]),
            // Before a pointing-device interaction, programmatic focus follows
            // the keyboard-visible default used by the focus-visible heuristic.
            keyboard_modality: Cell::new(true),
            selections: RefCell::new(HashMap::new()),
            editing: RefCell::new(EditingState::default()),
            programmatic_value_epoch: Cell::new(0),
            programmatic_value_writes: RefCell::new(ValueWriteJournal::default()),
        });
        let weak = Rc::downgrade(&realm);
        realm
            .session
            .borrow_mut()
            .document_mut()
            .set_mutation_sink(Some(Rc::new(move |document, mutation| {
                if let Some(realm) = weak.upgrade() {
                    let base_changed = realm.observe_base_url_mutation(document, mutation);
                    realm.observe_frame_navigation_mutation(document, mutation, base_changed);
                    realm.canvases.on_mutation(document, mutation);
                    realm.images.on_mutation(document, mutation);
                    realm.retain_dirty_image_request(mutation.target);
                    realm.scripts.borrow_mut().on_mutation(
                        document,
                        mutation,
                        realm.is_html_document,
                    );
                    range::adjust_ranges(&realm.ranges, document, mutation);
                    document_utilities::adjust_iterators(&realm.iterators, document, mutation);
                    if let Some(selection) = realm.selection.borrow().as_ref() {
                        range::sync_selection(selection);
                    }
                    let mut index = 0;
                    loop {
                        let Some(sink) = realm.mutation_sinks.borrow().get(index).cloned() else {
                            break;
                        };
                        sink(document, mutation);
                        index += 1;
                    }
                }
            })));
        for node in inert_parser_scripts {
            realm.scripts.borrow_mut().mark_started(node);
        }
        realm
    }

    pub(crate) fn document_value(self: &Rc<Self>, ctx: &mut Ctx) -> Value {
        if let Some(value) = self
            .document_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return value;
        }
        let document = DomDocument {
            base: DomNode {
                base: DomEventTarget::node(self, self.session.borrow().document().root()),
                realm: self.clone(),
                id: self.session.borrow().document().root(),
                collections: RefCell::new(HashMap::new()),
            },
            realm: self.clone(),
            fonts: RefCell::new(None),
        };
        let document = match self.document_interface {
            DocumentInterface::Document => ctx.new_instance(document),
            DocumentInterface::XmlDocument => ctx.new_instance(DomXmlDocument { base: document }),
        };
        ctx.set_native_identity_owner::<DomNode>(&document)
            .ok()
            .expect("Document wrapper has its native identity owner");
        *self.document_wrapper.borrow_mut() = ctx.weak_value(&document);
        document
    }
}

struct CurrentScriptRetention {
    owner: Rc<DomRealm>,
    node: NodeId,
}

struct ImageRequestRoot {
    // TargetData points back to its realm weakly, so storing it here preserves
    // listeners without making a realm -> wrapper -> realm strong cycle.
    target: Rc<TargetData>,
    retention: NodeRetention,
}

struct PendingScriptActivation {
    script: ScriptDescriptor,
    base_url: String,
    target: Rc<TargetData>,
    retention: NodeRetention,
}

/// A module preparation snapshot and lease on its native event target. The host
/// owns evaluation and scheduling; this object does not evaluate JavaScript.
pub struct ScriptActivation {
    pub script: ScriptDescriptor,
    pub base_url: String,
    // Drop the node retention while the original realm is still alive, so
    // adoption mappings remain available when releasing its counted root.
    _retention: NodeRetention,
    _target: Rc<TargetData>,
    _wrapper: Value,
    realm: Rc<DomRealm>,
}

impl ScriptActivation {
    /// Fire the element event only after actual graph evaluation/fetch finishes.
    /// The retained wrapper follows cross-document adoption before dispatch.
    pub fn dispatch_terminal(&self, ctx: &mut Ctx, kind: &str) -> OpResult<bool> {
        if !matches!(kind, "load" | "error") {
            return Err(OpError::type_error("invalid script terminal event type"));
        }
        let event = DomEvent::new(ctx, kind, None)?;
        let event = lumen::embed::JsObject::from_value(ctx.new_instance(event))
            .expect("native script Event object");
        events::dispatch_user_agent_event(ctx, lumen_bind::This(self._wrapper.clone()), event)
    }

    /// The current native owner/id for hosts which need the live element after
    /// adoption. Source and base URL stay those captured at preparation time.
    pub fn current_node(&self) -> (Rc<DomRealm>, NodeId) {
        self.realm.resolve_adopted_node(self.script.node)
    }
}

struct ImageEventRoot {
    // The scheduled event task is outside DomRealm, so it can keep the full
    // native wrapper alive without creating a self-cycle in the pending map.
    _wrapper: Value,
    _target: Rc<TargetData>,
    _retention: NodeRetention,
}

struct NodeRetention {
    realm: std::rc::Weak<DomRealm>,
    node: NodeId,
}

impl NodeRetention {
    fn new(realm: &Rc<DomRealm>, node: NodeId) -> Self {
        *realm.retained_nodes.borrow_mut().entry(node).or_default() += 1;
        Self {
            realm: Rc::downgrade(realm),
            node,
        }
    }

    fn adopt_nodes(
        &mut self,
        source: &Rc<DomRealm>,
        target: &Rc<DomRealm>,
        mapping: &[(NodeId, NodeId)],
    ) {
        if self
            .realm
            .upgrade()
            .is_some_and(|realm| Rc::ptr_eq(&realm, source))
        {
            if let Some((_, node)) = mapping.iter().find(|(old, _)| *old == self.node) {
                // migrate_adopted_state already moved this retained count into
                // the target document. Move only the token's owner/identity.
                self.realm = Rc::downgrade(target);
                self.node = *node;
            }
        }
    }
}

impl Drop for NodeRetention {
    fn drop(&mut self) {
        let Some(realm) = self.realm.upgrade() else {
            return;
        };
        let (realm, node) = realm.resolve_adopted_node(self.node);
        let mut retained = realm.retained_nodes.borrow_mut();
        if let Some(count) = retained.get_mut(&node) {
            *count -= 1;
            if *count == 0 {
                retained.remove(&node);
            }
        }
        drop(retained);

        let detached = {
            let session = realm.session.borrow();
            let document = session.document();
            node != document.root()
                && document.kind(node).is_ok()
                && document.parent(node).ok().flatten().is_none()
        };
        if detached {
            realm.reap_detached([node]);
        }
    }
}

/// Restores a document's previously active classic-script element when dropped.
pub struct CurrentScriptGuard {
    realm: Rc<DomRealm>,
    previous: Option<NodeId>,
    retention: Option<Rc<RefCell<CurrentScriptRetention>>>,
}

impl Drop for CurrentScriptGuard {
    fn drop(&mut self) {
        self.realm.current_script.set(self.previous);
        if let Some(retention) = self.retention.take() {
            let (realm, node) = {
                let retention = retention.borrow();
                (retention.owner.clone(), retention.node)
            };
            let token = Rc::downgrade(&retention);
            let mut script_retentions = realm.script_retentions.borrow_mut();
            let remove_entry = script_retentions.get_mut(&node).is_some_and(|active| {
                active.retain(|candidate| !candidate.ptr_eq(&token));
                active.is_empty()
            });
            if remove_entry {
                script_retentions.remove(&node);
            }
            drop(script_retentions);
            let mut retained = realm.retained_nodes.borrow_mut();
            if let Some(count) = retained.get_mut(&node) {
                *count -= 1;
                if *count == 0 {
                    retained.remove(&node);
                }
            }
            drop(retained);
            let detached = {
                let session = realm.session.borrow();
                let document = session.document();
                node != document.root()
                    && document.kind(node).is_ok()
                    && document.parent(node).ok().flatten().is_none()
            };
            if detached {
                realm.detached.borrow_mut().push(node);
            }
        }
    }
}

pub fn install(
    ctx: &mut Ctx,
    source: &str,
    max_nodes: usize,
) -> Result<Rc<DomRealm>, InstallError> {
    let _html_allocations = enter_html_allocation_category();
    ctx.set_none_is_null(true);
    let controller = dialog_popover::DetailsController::prepare(ctx).map_err(|_| InstallError::Global)?;
    let document = html::parse_with_options_initialized(
        source,
        max_nodes,
        html::ParseOptions {
            allow_declarative_shadow_roots: true,
            scripting_enabled: true,
        },
        |document| controller.attach(document),
    )
    .map_err(InstallError::Parse)?;
    let context = browsing_context::root_context(ctx).map_err(|_| InstallError::Global)?;
    install_document_with_context_metadata(
        ctx,
        document,
        "text/html",
        true,
        true,
        Some(context),
        None,
        None,
        None,
        true,
        Some(controller),
    )
}

/// Parse and install an active XHTML document.
///
/// Unlike [`install`], this uses the bounded XML parser: markup is required to
/// be well-formed, names retain their case, and external entities are never
/// fetched. The resulting XML Document still has a live browsing context.
pub fn install_xhtml(
    ctx: &mut Ctx,
    source: &str,
    max_nodes: usize,
) -> Result<Rc<DomRealm>, InstallError> {
    install_xml(ctx, source, max_nodes, XmlDocumentType::Xhtml)
}

/// Parse and install an active XML or standalone SVG document using the same
/// bounded parser and namespace-aware DOM arena as DOMParser and XHTML.
/// This installs the browsing context; painting is supplied by RenderSession.
pub fn install_xml(
    ctx: &mut Ctx,
    source: &str,
    max_nodes: usize,
    document_type: XmlDocumentType,
) -> Result<Rc<DomRealm>, InstallError> {
    let _html_allocations = enter_html_allocation_category();
    let controller = dialog_popover::DetailsController::prepare(ctx).map_err(|_| InstallError::Global)?;
    let document = lumen_html::xml::parse_initialized(source, max_nodes, |document| controller.attach(document))
        .map_err(InstallError::XmlParse)?;
    let context = browsing_context::root_context(ctx).map_err(|_| InstallError::Global)?;
    install_document_with_context_metadata(
        ctx,
        document,
        document_type.content_type(),
        false,
        true,
        Some(context),
        None,
        None,
        None,
        true,
        Some(controller),
    )
}

fn install_document(
    ctx: &mut Ctx,
    document: lumen_html::Document,
    content_type: &str,
    is_html_document: bool,
    has_browsing_context: bool,
    context: Option<Rc<browsing_context::BrowsingContext>>,
    document_url: Option<String>,
    about_base_url: Option<String>,
) -> Result<Rc<DomRealm>, InstallError> {
    install_document_with_context_metadata(
        ctx,
        document,
        content_type,
        is_html_document,
        has_browsing_context,
        context,
        document_url,
        about_base_url,
        None,
        true,
        None,
    )
}

pub(crate) fn install_document_staged(
    ctx: &mut Ctx,
    document: lumen_html::Document,
    content_type: &str,
    is_html_document: bool,
    context: Rc<browsing_context::BrowsingContext>,
    metadata: Rc<browsing_context::RealmMetadata>,
    document_url: String,
    about_base_url: Option<String>,
    details_controller: Rc<dialog_popover::DetailsController>,
) -> Result<Rc<DomRealm>, InstallError> {
    install_document_with_context_metadata(
        ctx,
        document,
        content_type,
        is_html_document,
        true,
        Some(context),
        Some(document_url),
        about_base_url,
        Some(metadata),
        false,
        Some(details_controller),
    )
}

fn install_document_with_context_metadata(
    ctx: &mut Ctx,
    mut document: lumen_html::Document,
    content_type: &str,
    is_html_document: bool,
    has_browsing_context: bool,
    context: Option<Rc<browsing_context::BrowsingContext>>,
    document_url: Option<String>,
    about_base_url: Option<String>,
    context_metadata: Option<Rc<browsing_context::RealmMetadata>>,
    publish_global_this: bool,
    prepared_details: Option<Rc<dialog_popover::DetailsController>>,
) -> Result<Rc<DomRealm>, InstallError> {
    ctx.set_none_is_null(true);
    let controller = match prepared_details {
        Some(controller) => controller,
        None => dialog_popover::DetailsController::prepare(ctx).map_err(|_| InstallError::Global)?,
    };
    controller.attach(&mut document);
    document.set_html_document(is_html_document);
    let target_module = ctx
        .module_object::<events::target_bindings::Module>()
        .map_err(|_| InstallError::Global)?;
    let global = ctx.global_object();
    ctx.member_set(&global, "__dom_event_targets", target_module)
        .map_err(|_| InstallError::Global)?;
    let realm = DomRealm::realm_from_document_with_metadata(
        document,
        content_type,
        is_html_document,
        has_browsing_context,
    );
    realm_services::RealmServices::replace_shared_current(ctx, realm.script_capabilities.clone());
    if let Some(origin) = context_metadata
        .as_ref()
        .map(|metadata| browsing_context::metadata_origin(metadata))
        .or_else(|| {
            context
                .as_ref()
                .map(|context| browsing_context::context_origin(context))
        })
    {
        realm.set_document_origin(origin);
    }
    let window_proxy = if let Some(context) = context.as_ref() {
        if let Some(metadata) = context_metadata.as_ref() {
            browsing_context::register_context_service_with_metadata(
                ctx,
                context.clone(),
                metadata.clone(),
            );
        } else {
            browsing_context::register_context_service(ctx, context.clone());
        }
        *realm.browsing_context.borrow_mut() = Rc::downgrade(context);
        if let Some(base_url) = about_base_url {
            realm.set_about_base_url(Some(base_url));
        }
        if let Some(document_url) = document_url {
            realm.set_document_url(document_url);
        }
        let proxy = browsing_context::create_context_proxy(ctx, context)
            .map_err(|_| InstallError::Global)?;
        if publish_global_this {
            let handle = browsing_context::context_realm_handle(context);
            ctx.set_host_global_this(&handle, proxy.clone())
                .map_err(|_| InstallError::Global)?;
        }
        *realm.window_wrapper.borrow_mut() = ctx.weak_value(&proxy);
        Some(proxy)
    } else {
        None
    };
    // Establish the parsed document's active base before mutation observers are
    // connected, so synchronous base edits compare with the pre-mutation URL.
    let _ = realm.base_url();
    if has_browsing_context {
        let parser_scripts = {
            let session = realm.session.borrow();
            script_loading::parser_scripts(session.document())
        };
        let template_scripts = {
            let session = realm.session.borrow();
            script_loading::template_scripts(session.document())
        };
        let mut scripts = realm.scripts.borrow_mut();
        for node in parser_scripts {
            scripts.register_parser_script(node);
        }
        for node in template_scripts {
            scripts.mark_started(node);
        }
    }
    let global = ctx.global_object();
    // Materialize a host's lazy web-event unit before replacing its DOM classes.
    // Minimal native hosts have no such unit; the DOM installs its own classes.
    if ctx.has_own_property_value(&global, &Value::str("EventTarget"))
        .map_err(|_| InstallError::Global)? {
        ctx.get_member(&global, "EventTarget")
            .map_err(|_| InstallError::Global)?;
    }
    let window_target = DomEventTarget::window(&realm);
    *realm.window_target.borrow_mut() = Some(window_target.data_handle());
    realm.font_loading.capture_dom_exception(ctx);
    ctx.attach_instance(
        &global,
        window_globals::DomWindow::from_target(window_target),
    )
    .map_err(|_| InstallError::Global)?;
    let node = ctx.class_constructor::<DomNode>();
    install_node_constants(ctx, &node)?;
    let document_class = ctx.class_constructor::<DomDocument>();
    let document = realm.document_value(ctx);
    // Iterator objects have a native prototype but no exposed global constructor.
    let _ = ctx.class_constructor::<font_loading::DomFontFaceSetIterator>();
    for (name, ctor) in [
        ("EventTarget", ctx.class_constructor::<DomEventTarget>()),
        (
            "Window",
            ctx.class_constructor::<window_globals::DomWindow>(),
        ),
        (
            "Location",
            ctx.class_constructor::<window_globals::DomLocation>(),
        ),
        ("Event", ctx.class_constructor::<DomEvent>()),
        ("UIEvent", ctx.class_constructor::<ui_events::DomUIEvent>()),
        (
            "FocusEvent",
            ctx.class_constructor::<ui_events::DomFocusEvent>(),
        ),
        (
            "MouseEvent",
            ctx.class_constructor::<ui_events::DomMouseEvent>(),
        ),
        (
            "PointerEvent",
            ctx.class_constructor::<ui_events::DomPointerEvent>(),
        ),
        (
            "WheelEvent",
            ctx.class_constructor::<ui_events::DomWheelEvent>(),
        ),
        (
            "KeyboardEvent",
            ctx.class_constructor::<ui_events::DomKeyboardEvent>(),
        ),
        (
            "CompositionEvent",
            ctx.class_constructor::<ui_events::DomCompositionEvent>(),
        ),
        (
            "InputEvent",
            ctx.class_constructor::<ui_events::DomInputEvent>(),
        ),
        (
            "SubmitEvent",
            ctx.class_constructor::<events::DomSubmitEvent>(),
        ),
        (
            "FormDataEvent",
            ctx.class_constructor::<events::DomFormDataEvent>(),
        ),
        (
            "ErrorEvent",
            ctx.class_constructor::<ui_events::DomErrorEvent>(),
        ),
        (
            "PromiseRejectionEvent",
            ctx.class_constructor::<events::DomPromiseRejectionEvent>(),
        ),
        (
            "FontFace",
            ctx.class_constructor::<font_loading::DomFontFace>(),
        ),
        (
            "FontFaceSet",
            ctx.class_constructor::<font_loading::DomFontFaceSet>(),
        ),
        (
            "FontFaceSetLoadEvent",
            ctx.class_constructor::<font_loading::DomFontFaceSetLoadEvent>(),
        ),
        (
            "DOMImplementation",
            ctx.class_constructor::<DomImplementation>(),
        ),
        ("Attr", ctx.class_constructor::<attributes::DomAttr>()),
        (
            "NamedNodeMap",
            ctx.class_constructor::<attributes::DomNamedNodeMap>(),
        ),
        ("NodeList", ctx.class_constructor::<DomNodeList>()),
        (
            "HTMLCollection",
            ctx.class_constructor::<DomHtmlCollection>(),
        ),
        (
            "HTMLFormControlsCollection",
            ctx.class_constructor::<DomHtmlFormControlsCollection>(),
        ),
        (
            "RadioNodeList",
            ctx.class_constructor::<collections::DomRadioNodeList>(),
        ),
        (
            "HTMLOptionsCollection",
            ctx.class_constructor::<DomHtmlOptionsCollection>(),
        ),
        ("DOMTokenList", ctx.class_constructor::<DomTokenList>()),
        (
            "ValidityState",
            ctx.class_constructor::<forms::DomValidityState>(),
        ),
        ("CSSStyleDeclaration", ctx.class_constructor::<DomStyle>()),
        ("Element", ctx.class_constructor::<DomElement>()),
        ("HTMLElement", ctx.class_constructor::<DomHtmlElement>()),
        (
            "HTMLAnchorElement",
            ctx.class_constructor::<hyperlinks::DomAnchorElement>(),
        ),
        (
            "HTMLAreaElement",
            ctx.class_constructor::<hyperlinks::DomAreaElement>(),
        ),
        (
            "HTMLHtmlElement",
            ctx.class_constructor::<DomHtmlHtmlElement>(),
        ),
        (
            "HTMLHeadElement",
            ctx.class_constructor::<DomHtmlHeadElement>(),
        ),
        (
            "HTMLBodyElement",
            ctx.class_constructor::<DomHtmlBodyElement>(),
        ),
        (
            "HTMLTitleElement",
            ctx.class_constructor::<DomHtmlTitleElement>(),
        ),
        (
            "HTMLBaseElement",
            ctx.class_constructor::<DomHtmlBaseElement>(),
        ),
        (
            "HTMLLinkElement",
            ctx.class_constructor::<DomHtmlLinkElement>(),
        ),
        (
            "HTMLScriptElement",
            ctx.class_constructor::<DomHtmlScriptElement>(),
        ),
        (
            "HTMLImageElement",
            ctx.class_constructor::<DomHtmlImageElement>(),
        ),
        (
            "HTMLMediaElement",
            ctx.class_constructor::<media::DomHtmlMediaElement>(),
        ),
        (
            "HTMLAudioElement",
            ctx.class_constructor::<media::DomHtmlAudioElement>(),
        ),
        (
            "HTMLVideoElement",
            ctx.class_constructor::<media::DomHtmlVideoElement>(),
        ),
        ("DOMStringMap", ctx.class_constructor::<DomDomStringMap>()),
        (
            "HTMLCanvasElement",
            ctx.class_constructor::<canvas::DomCanvasElement>(),
        ),
        (
            "HTMLStyleElement",
            ctx.class_constructor::<DomStyleElement>(),
        ),
        ("HTMLFormElement", ctx.class_constructor::<DomFormElement>()),
        (
            "HTMLDetailsElement",
            ctx.class_constructor::<DomDetailsElement>(),
        ),
        (
            "HTMLIFrameElement",
            ctx.class_constructor::<DomIFrameElement>(),
        ),
        (
            "HTMLInputElement",
            ctx.class_constructor::<DomInputElement>(),
        ),
        (
            "HTMLSelectElement",
            ctx.class_constructor::<DomSelectElement>(),
        ),
        (
            "HTMLOptionElement",
            ctx.class_constructor::<DomOptionElement>(),
        ),
        (
            "HTMLTextAreaElement",
            ctx.class_constructor::<DomTextAreaElement>(),
        ),
        (
            "Animation",
            ctx.class_constructor::<animations::DomAnimation>(),
        ),
        (
            "CSSAnimation",
            ctx.class_constructor::<animations::DomCssAnimation>(),
        ),
        (
            "KeyframeEffect",
            ctx.class_constructor::<animations::DomKeyframeEffect>(),
        ),
        (
            "DocumentTimeline",
            ctx.class_constructor::<animations::DomDocumentTimeline>(),
        ),
        ("FileList", ctx.class_constructor::<forms::DomFileList>()),
        ("HTMLSlotElement", ctx.class_constructor::<DomSlotElement>()),
        ("ShadowRoot", ctx.class_constructor::<DomShadowRoot>()),
        (
            "HTMLTemplateElement",
            ctx.class_constructor::<DomTemplateElement>(),
        ),
        ("CharacterData", ctx.class_constructor::<DomCharacterData>()),
        ("Text", ctx.class_constructor::<DomText>()),
        ("CDATASection", ctx.class_constructor::<DomCDataSection>()),
        ("Comment", ctx.class_constructor::<DomComment>()),
        (
            "ProcessingInstruction",
            ctx.class_constructor::<DomProcessingInstruction>(),
        ),
        ("DocumentType", ctx.class_constructor::<DomDocumentType>()),
        (
            "DocumentFragment",
            ctx.class_constructor::<DomDocumentFragment>(),
        ),
        ("XMLDocument", ctx.class_constructor::<DomXmlDocument>()),
    ] {
        crate::install_interface(ctx, &global, name, ctor).map_err(|_| InstallError::Global)?;
    }
    for (name, constructor) in html_interfaces::constructors(ctx) {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| InstallError::Global)?;
    }
    for (name, constructor) in dialog_popover::constructors(ctx) {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| InstallError::Global)?;
    }
    let keyboard_constructor = ctx.class_constructor::<ui_events::DomKeyboardEvent>();
    let wheel_constructor = ctx.class_constructor::<ui_events::DomWheelEvent>();
    let event_constructor = ctx.class_constructor::<DomEvent>();
    ui_events::install_ui_event_constants(
        ctx,
        &event_constructor,
        &keyboard_constructor,
        &wheel_constructor,
    )
    .map_err(|_| InstallError::Global)?;
    ctx.class_constructor::<DomCollectionIterator>();
    let computed_style =
        ctx.bound_function(&lumen_bind::FnItem::of::<style::get_computed_style::Op>());
    ctx.set_member(&global, "getComputedStyle", computed_style)
        .map_err(|_| InstallError::Global)?;
    let runtime = reactive::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "__lumen", runtime)
        .map_err(|_| InstallError::Global)?;
    for (name, constructor) in document_utilities::constructors(ctx) {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| InstallError::Global)?;
    }
    let div_constructor = ctx.class_constructor::<DomHtmlDivElement>();
    crate::install_interface(
        ctx,
        &global,
        lumen::embed::class_name::<DomHtmlDivElement>(),
        div_constructor,
    )
    .map_err(|_| InstallError::Global)?;
    let br_constructor = ctx.class_constructor::<DomHtmlBrElement>();
    crate::install_interface(
        ctx,
        &global,
        lumen::embed::class_name::<DomHtmlBrElement>(),
        br_constructor,
    )
    .map_err(|_| InstallError::Global)?;
    for (name, constructor) in range::constructors(ctx) {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| InstallError::Global)?;
    }
    range::install_range_constants(ctx).map_err(|_| InstallError::Global)?;
    range::install_node_filter(ctx).map_err(|_| InstallError::Global)?;
    observers::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    geometry::install(ctx).map_err(|_| InstallError::Global)?;
    layout_observers::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    cssom::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    forms::install(ctx).map_err(|_| InstallError::Global)?;
    animations::install(ctx, &realm);
    webaudio::install(ctx);
    form_data_bridge::install(ctx).map_err(|_| InstallError::Global)?;
    scheduling::install(ctx).map_err(|_| InstallError::Global)?;
    window_globals::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    dataset::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    browser_services::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    media_capture::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    webrtc::install(ctx).map_err(|_| InstallError::Global)?;
    notifications::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    object_urls::install(ctx).map_err(|_| InstallError::Global)?;
    custom_elements::install(ctx, realm.clone()).map_err(|_| InstallError::Global)?;
    canvas::install(ctx).map_err(|_| InstallError::Global)?;
    crate::install_interface(ctx, &global, "Node", node).map_err(|_| InstallError::Global)?;
    let image_constructor = ctx.class_constructor::<DomHtmlImageElement>();
    crate::install_interface(ctx, &global, "Image", image_constructor)
        .map_err(|_| InstallError::Global)?;
    let audio_constructor = ctx.class_constructor::<media::DomHtmlAudioElement>();
    crate::install_interface(ctx, &global, "Audio", audio_constructor)
        .map_err(|_| InstallError::Global)?;
    crate::install_interface(ctx, &global, "Document", document_class)
        .map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "document", document)
        .map_err(|_| InstallError::Global)?;
    if let Some(window_proxy) = window_proxy {
        ctx.set_member(&global, "window", window_proxy.clone())
            .map_err(|_| InstallError::Global)?;
        ctx.set_member(&global, "self", window_proxy)
            .map_err(|_| InstallError::Global)?;
    }
    error_reporting::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    event_content_handlers::initialize_document(ctx, &realm).map_err(|_| InstallError::Global)?;
    if publish_global_this {
        if let Some(context) = context.as_ref() {
            browsing_context::bind_context_document(context, &realm);
        }
    }
    controller.bind(ctx, &realm);
    Ok(realm)
}

/// Notify media-query listeners after the embedder updates layout/environment.
pub fn notify_media(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    cssom::notify_media(ctx, realm)
}

/// Web IDL interface objects are writable/configurable, non-enumerable globals.
/// Use intrinsic property definition so installation never calls author setters.
pub(crate) fn install_interface(
    ctx: &mut Ctx,
    global: &Value,
    name: &str,
    constructor: Value,
) -> Result<(), Value> {
    let descriptor = ctx.new_object_with_proto(&Value::Null);
    for (key, value) in [
        ("value", constructor),
        ("writable", Value::Bool(true)),
        ("enumerable", Value::Bool(false)),
        ("configurable", Value::Bool(true)),
    ] {
        ctx.member_set(&descriptor, key, value)?;
    }
    ctx.define_property_value(global, Value::str(name), &descriptor)
}

fn install_node_constants(ctx: &mut Ctx, constructor: &Value) -> Result<(), InstallError> {
    let prototype = ctx
        .get_member(constructor, "prototype")
        .map_err(|_| InstallError::Global)?;
    // WebIDL constants are enumerable, non-writable, non-configurable own
    // properties of both the interface object and its prototype.
    for (name, value) in [
        ("ELEMENT_NODE", 1),
        ("ATTRIBUTE_NODE", 2),
        ("TEXT_NODE", 3),
        ("CDATA_SECTION_NODE", 4),
        ("ENTITY_REFERENCE_NODE", 5),
        ("ENTITY_NODE", 6),
        ("PROCESSING_INSTRUCTION_NODE", 7),
        ("COMMENT_NODE", 8),
        ("DOCUMENT_NODE", 9),
        ("DOCUMENT_TYPE_NODE", 10),
        ("DOCUMENT_FRAGMENT_NODE", 11),
        ("NOTATION_NODE", 12),
        ("DOCUMENT_POSITION_DISCONNECTED", 1),
        ("DOCUMENT_POSITION_PRECEDING", 2),
        ("DOCUMENT_POSITION_FOLLOWING", 4),
        ("DOCUMENT_POSITION_CONTAINS", 8),
        ("DOCUMENT_POSITION_CONTAINED_BY", 16),
        ("DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC", 32),
    ] {
        let descriptor = Value::Obj(ctx.new_object());
        for (key, value) in [
            ("value", Value::Num(value as f64)),
            ("writable", Value::Bool(false)),
            ("enumerable", Value::Bool(true)),
            ("configurable", Value::Bool(false)),
        ] {
            ctx.set_member(&descriptor, key, value)
                .map_err(|_| InstallError::Global)?;
        }
        for target in [constructor, &prototype] {
            ctx.define_property_value(target, Value::Str(name.into()), &descriptor)
                .map_err(|_| InstallError::Global)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_html_image::render_with_font;
    use lumen_html_text::{FontFace, DEFAULT_FONT_BYTES};
    use std::sync::Arc;

    fn script(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid script") {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "message")
                    .ok()
                    .and_then(|value| {
                        if let Value::Str(message) = value {
                            Some(message.to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_else(|| "script threw".into());
                panic!("{message}");
            }
        }
    }

    #[test]
    fn initial_blank_iframe_load_is_synchronous_and_one_shot() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let realm = install(
            engine.ctx(),
            "<body><iframe onload=\"globalThis.parserBlankLoads=(globalThis.parserBlankLoads||0)+1\"></iframe></body>",
            256,
        )
        .unwrap();
        realm.process_initial_iframe_post_connections(engine.ctx());
        realm.process_initial_iframe_post_connections(engine.ctx());
        assert!(matches!(
            script(engine, "globalThis.parserBlankLoads === 1"),
            Value::Bool(true)
        ));

        let dynamic = script(
            engine,
            r#"
            (() => {
              let loads = 0;
              let trusted = true;
              let rightTarget = true;
              const observe = frame => frame.addEventListener('load', event => {
                loads++;
                trusted = trusted && event.isTrusted;
                rightTarget = rightTarget && event.target === frame &&
                  event.currentTarget === frame;
              });

              const direct = document.createElement('iframe');
              observe(direct);
              document.body.appendChild(direct);
              const directWasSync = loads === 1;
              void direct.contentWindow;
              void direct.contentDocument;
              const readsDidNotRepeat = loads === 1;
              direct.remove();
              document.body.appendChild(direct);
              const reinsertedOnce = loads === 2;

              const fragmentFrame = document.createElement('iframe');
              observe(fragmentFrame);
              const fragment = document.createDocumentFragment();
              fragment.appendChild(fragmentFrame);
              document.body.appendChild(fragment);
              const fragmentWasSync = loads === 3;

              const host = document.createElement('div');
              document.body.appendChild(host);
              host.innerHTML = '<iframe onload="globalThis.markupBlankLoads=(globalThis.markupBlankLoads||0)+1"></iframe>';
              const markupWasSync = globalThis.markupBlankLoads === 1;

              const deferred = document.createElement('iframe');
              deferred.srcdoc = '';
              const srcdocReflected = deferred.srcdoc === '' && deferred.getAttribute('srcdoc') === '';
              let deferredLoads = 0;
              deferred.addEventListener('load', () => deferredLoads++);
              document.body.appendChild(deferred);
              const srcdocDidNotUseBlankLoad = deferredLoads === 0;

              const remote = document.createElement('iframe');
              remote.src = '/later.html';
              const srcReflected = remote.getAttribute('src') === '/later.html';
              let remoteLoads = 0;
              remote.addEventListener('load', () => remoteLoads++);
              document.body.appendChild(remote);
              const urlDidNotUseBlankLoad = remoteLoads === 0;

              const destination = document.createElement('iframe');
              document.body.appendChild(destination);
              const destinationDocument = destination.contentDocument;
              const adopted = document.createElement('iframe');
              let adoptedLoads = 0;
              adopted.addEventListener('load', () => {
                adoptedLoads++;
                if (adoptedLoads === 1) {
                  destinationDocument.adoptNode(adopted);
                  destinationDocument.body.appendChild(adopted);
                }
              });
              document.body.appendChild(adopted);
              const callbackAdoptionWasSafe = adoptedLoads === 2 &&
                adopted.ownerDocument === destinationDocument && adopted.contentDocument !== null;

              const failures = [];
              if (!directWasSync) failures.push('direct append did not dispatch synchronously');
              if (!readsDidNotRepeat) failures.push('contentWindow/contentDocument repeated the load');
              if (!reinsertedOnce) failures.push('detach/reinsert did not create one fresh load');
              if (!fragmentWasSync) failures.push('fragment insertion did not dispatch synchronously');
              if (!markupWasSync) failures.push('innerHTML insertion did not dispatch synchronously');
              if (!srcdocDidNotUseBlankLoad) failures.push('srcdoc dispatched an initial blank load');
              if (!srcdocReflected) failures.push('srcdoc IDL did not reflect its null-namespace attribute');
              if (!urlDidNotUseBlankLoad) failures.push('URL navigation dispatched an initial blank load');
              if (!srcReflected) failures.push('src IDL did not reflect its null-namespace attribute');
              if (loads !== 3) failures.push('unexpected blank load count');
              if (!trusted) failures.push('initial blank load was not trusted');
              if (!rightTarget) failures.push('initial blank load target/currentTarget mismatch');
              if (!callbackAdoptionWasSafe) failures.push('load callback adoption did not rehome one fresh navigable');
              return failures.join('; ') || 'ok';
            })()
            "#,
        );
        match dynamic {
            Value::Str(result) if result.as_str() == "ok" => {}
            Value::Str(failures) => panic!("initial blank iframe failures: {failures}"),
            _ => panic!("initial blank iframe diagnostic did not return a string"),
        }
    }

    #[test]
    fn invalid_selectors_throw_captured_dom_syntax_errors() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main><div></div></main>", 64).unwrap();
        let result = script(
            engine,
            r#"
                const NativeDOMException = DOMException;
                globalThis.DOMException = class ReplacedDOMException {};
                const invalid = [
                    () => document.querySelector('cp|coreProperties'),
                    () => document.querySelectorAll('[xml|lang]'),
                    () => document.querySelector('main').matches('cp|coreProperties'),
                    () => document.querySelector('main').closest('[xml|lang]')
                ];
                const domErrors = invalid.map(run => {
                    try { run(); return null; }
                    catch (error) { return error; }
                });
                let parserError;
                try { new Function('let ='); }
                catch (error) { parserError = error; }
                domErrors.length === 4 && domErrors.every(error =>
                    error instanceof NativeDOMException &&
                    error.name === 'SyntaxError' && error.code === 12
                ) && parserError instanceof SyntaxError &&
                    parserError.code === undefined &&
                    !(parserError instanceof NativeDOMException)
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_dom_classes_inherit_and_dispatch_base_methods() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div>x<!--y--></div>", 64).unwrap();
        let value = engine.eval_value("const div = document.querySelector('div'); const text = div.firstChild; const comment = div.lastChild; div instanceof HTMLElement && div instanceof Element && div instanceof Node && div instanceof EventTarget && text instanceof Text && text instanceof CharacterData && text instanceof Node && comment instanceof Comment && document instanceof Document && document instanceof Node && document.nodeType === 9 && text.data === 'x' && Text.prototype instanceof CharacterData && Object.getPrototypeOf(HTMLElement) === Element && document.createDocumentFragment() instanceof DocumentFragment").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn document_forms_and_form_elements_are_live_named_collections() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<form id='f' name='namedForm'><input type='radio' id='a' name='group' value='A'><input type='radio' id='b' name='group' value='B'><input id='third' name='group'><input id='by-name' name='other'><input name='mixed'><input id='mixed'><input id='duplicate'><input id='duplicate'><input type='image' id='image' name='excluded'></form><input id='external' type='radio' name='group' form='f'>",
            128,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
                const expect = (condition, label) => {
                    if (!condition) throw new Error(label);
                };
                const forms = document.forms;
                const form = forms.namedForm;
                const elements = form.elements;
                const named = elements.group;
                const firstRadio = elements.namedItem('group');
                const byName = elements.namedItem('other');
                const excluded = document.getElementById('image');
                expect(forms === document.forms && forms.length === 1 &&
                    forms.item(0) === form && form instanceof HTMLFormElement &&
                    forms.namedItem('f') === form && forms.namedItem('namedForm') === form &&
                    forms.namedItem('') === null &&
                    forms instanceof HTMLCollection && !(forms instanceof NodeList) &&
                    Object.getPrototypeOf(HTMLCollection.prototype) === Object.prototype,
                    'document.forms collection identity and prototype');
                expect(elements === form.elements &&
                    elements instanceof HTMLFormControlsCollection &&
                    elements instanceof HTMLCollection && !(elements instanceof NodeList) &&
                    elements.length === 9 && elements[0].id === 'a' &&
                    elements[8].id === 'external',
                    'form.elements type, length, and tree order');
                expect(named instanceof RadioNodeList && named instanceof NodeList &&
                    named.length === 4 && firstRadio instanceof RadioNodeList &&
                    byName.id === 'by-name',
                    'form named properties and single-name lookup');
                expect(elements.namedItem('mixed') instanceof RadioNodeList &&
                    elements.namedItem('mixed').length === 2 &&
                    elements.namedItem('duplicate') instanceof RadioNodeList &&
                    elements.namedItem('duplicate').length === 2,
                    'mixed id/name and duplicate-id RadioNodeList lookup');
                expect(!Array.from(elements).includes(excluded),
                    'image submitter exclusion and HTMLCollection iteration');
                document.getElementById('b').checked = true;
                expect(named.value === 'B', 'RadioNodeList value getter');
                named.value = 'A';
                expect(document.getElementById('a').checked &&
                    !document.getElementById('b').checked,
                    'RadioNodeList value setter');
                document.getElementById('a').remove();
                expect(elements.length === 8 && named.length === 3 &&
                    elements.namedItem('group') instanceof RadioNodeList,
                    'live collection after removal');
                const added = document.createElement('form');
                added.id = 'later';
                document.body.appendChild(added);
                const earlierName = document.createElement('form');
                earlierName.id = 'earlier-name';
                earlierName.name = 'collision';
                document.body.appendChild(earlierName);
                const laterId = document.createElement('form');
                laterId.id = 'collision';
                document.body.appendChild(laterId);
                const collision = document.createElement('form');
                collision.id = 'length';
                document.body.appendChild(collision);
                expect(earlierName.getAttribute('name') === 'collision',
                    'HTMLFormElement.name reflects its attribute');
                expect(forms.length === 5 && forms.later === added,
                    'live forms collection and named property');
                expect(forms.namedItem('collision') === earlierName &&
                    forms.collision === earlierName,
                    'namedItem uses first tree-order id-or-name match');
                expect(forms.namedItem('length') === collision &&
                    typeof forms.length === 'number',
                    'built-in length shadows named property');
                true
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn dir_idl_reflection_tracks_known_keywords_and_document_root() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<div id=target></div>", 64).unwrap();
        let value = script(
            engine,
            r#"(() => {
                const target = document.getElementById('target');
                if (target.dir !== '' || document.dir !== '') throw new Error('initial dir reflection');
                target.dir = 'RtL';
                if (target.dir !== 'rtl' || target.getAttribute('dir') !== 'RtL' ||
                    !target.matches(':dir(rtl)')) throw new Error('element dir reflection');
                target.dir = 'invalid';
                if (target.dir !== '' || target.getAttribute('dir') !== 'invalid') throw new Error('invalid element dir reflection');
                document.dir = 'RTL';
                if (document.dir !== 'rtl' || document.documentElement.dir !== 'rtl' ||
                    !target.matches(':dir(rtl)')) throw new Error('document dir inheritance');
                let calls = 0;
                document.dir = {toString() { calls++; return 'auto'; }};
                if (calls !== 1 || document.dir !== 'auto') throw new Error('document dir coercion');
                document.dir = ' auto ';
                if (document.dir !== '' || document.documentElement.getAttribute('dir') !== ' auto ')
                    throw new Error('invalid document dir reflection');
                const xml = new DOMParser().parseFromString('<root/>', 'application/xml');
                xml.dir = 'rtl';
                if (xml.dir !== '' || xml.documentElement.hasAttribute('dir')) throw new Error('XML dir setter');
                const empty = document.implementation.createDocument(null, '', null);
                empty.dir = 'rtl';
                if (empty.dir !== '' || empty.documentElement !== null) throw new Error('empty document dir setter');
                const form = document.createElement('form');
                form.dir = 'rtl';
                const input = document.createElement('input');
                input.name = 'field';
                input.dirName = 'field.dir';
                input.value = 'value';
                form.appendChild(input);
                const area = document.createElement('textarea');
                area.name = 'area';
                area.dirName = 'area.dir';
                form.appendChild(area);
                const select = document.createElement('select');
                select.name = 'choice';
                const option = document.createElement('option');
                option.value = 'selected';
                select.appendChild(option);
                form.appendChild(select);
                const data = new FormData(form);
                const valid = input.dirName === 'field.dir' && area.dirName === 'area.dir' &&
                    input.getAttribute('dirname') === 'field.dir' &&
                    area.getAttribute('name') === 'area' && select.getAttribute('name') === 'choice' &&
                    data.get('field.dir') === 'rtl' && data.get('area.dir') === 'rtl' &&
                    data.get('choice') === 'selected';
                if (!valid) throw new Error('dirname reflection/entries: ' + JSON.stringify([
                    input.dirName, area.dirName, input.getAttribute('dirname'),
                    data.get('field.dir'), data.get('area.dir')]));
                return true;
            })()"#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn html_form_legacy_indexed_named_properties_and_boolean_setters() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<form id='f'><input id='field' name='single'><input id='first' name='group'><input id='second' name='group'><input id='addEventListener'><input id='oldName'><input id='imageButton' type='image'><img id='imageOnly' name='imageGroup' form='other'><img id='imageSecond' name='imageGroup'></form><form id='other'></form><details></details><button></button>",
            128,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
                const form = document.getElementById('f');
                const field = document.getElementById('field');
                const collidingMethod = document.getElementById('addEventListener');
                const imageOnly = document.getElementById('imageOnly');
                const imageSecond = document.getElementById('imageSecond');
                const namedKeys = () => Object.getOwnPropertyNames(form);
                const indexDescriptor = Object.getOwnPropertyDescriptor(form, '0');
                const nameDescriptor = Object.getOwnPropertyDescriptor(form, 'single');
                const ownExpando = Object.defineProperty(form, 'future', {
                    value: 'own', writable: true, enumerable: true, configurable: true
                });
                const future = document.createElement('input');
                future.id = 'future';
                form.appendChild(future);
                const oldName = document.getElementById('oldName');
                const oldNameWasRemembered = form.oldName === oldName;
                oldName.id = 'newName';
                const pastNameStillWorks = form.oldName === oldName && form.newName === oldName;
                const changedType = document.createElement('input');
                changedType.id = 'previousTypeName';
                form.appendChild(changedType);
                const oldTypeNameWasRemembered = form.previousTypeName === changedType;
                changedType.setAttribute('type', 'image');
                const pastNameSurvivesTypeChange = form.previousTypeName === changedType &&
                    namedKeys().includes('previousTypeName');
                const group = form.group;
                const namedSetCreatesOwnExpando = (() => {
                    try {
                        (function() { 'use strict'; form.single = 'replace'; })();
                        return false;
                    } catch (error) {
                        return error instanceof TypeError && form.single === field &&
                            Reflect.set(form, 'single', 'x') === false;
                    }
                })();
                const ownNamedExpandoDeletes = (() => {
                    form.fresh = 'expando';
                    return form.fresh === 'expando' && delete form.fresh &&
                        form.fresh === undefined;
                })();
                const blockedDelete = !delete form.single;
                const blockedDefine = (() => {
                    try { Object.defineProperty(form, 'single', {value: 'define'}); }
                    catch (error) { return error instanceof TypeError; }
                    return false;
                })();
                const noBooleanValueOf = { valueOf() { throw new Error('ToBoolean used ToNumber'); } };
                const option = document.createElement('option');
                const details = document.querySelector('details');
                const button = document.querySelector('button');
                option.selected = noBooleanValueOf;
                option.defaultSelected = noBooleanValueOf;
                details.open = noBooleanValueOf;
                button.formNoValidate = noBooleanValueOf;
                const booleanInput = document.createElement('input');
                booleanInput.checked = noBooleanValueOf;
                booleanInput.defaultChecked = noBooleanValueOf;
                const booleanSettersCoerce = option.selected && option.defaultSelected &&
                    details.open && button.formNoValidate && booleanInput.checked &&
                    booleanInput.defaultChecked;
                const listedLengthExcludesImages = form.length === 6 &&
                    form.elements.length === form.length;
                oldName.remove();
                form.single === field && form[0] === field && form[3] === collidingMethod &&
                    form.addEventListener === collidingMethod && listedLengthExcludesImages &&
                    form.imageOnly === imageOnly && form.imageGroup instanceof RadioNodeList &&
                    form.imageGroup.length === 2 && form.imageGroup[0] === imageOnly &&
                    form.imageGroup[1] === imageSecond &&
                    namedKeys().includes('imageOnly') &&
                    namedKeys().indexOf('imageOnly') < namedKeys().indexOf('imageGroup') &&
                    namedKeys().indexOf('imageGroup') < namedKeys().indexOf('imageSecond') &&
                    !namedKeys().includes('imageButton') && form.imageButton === undefined &&
                    !Object.keys(form).includes('imageOnly') &&
                    form.group instanceof RadioNodeList && group.length === 2 &&
                    form.group[0] === document.getElementById('first') &&
                    indexDescriptor.value === field && indexDescriptor.enumerable &&
                    !indexDescriptor.writable && indexDescriptor.configurable &&
                    nameDescriptor.value === field && !nameDescriptor.enumerable &&
                    !nameDescriptor.writable && nameDescriptor.configurable &&
                    Object.getOwnPropertyNames(form).includes('single') &&
                    !Object.keys(form).includes('single') &&
                    namedSetCreatesOwnExpando && ownNamedExpandoDeletes && blockedDelete &&
                    blockedDefine &&
                    oldNameWasRemembered && pastNameStillWorks && oldTypeNameWasRemembered &&
                    pastNameSurvivesTypeChange && form.oldName === undefined &&
                    form.future === 'own' && booleanSettersCoerce
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn attrs_are_live_identity_objects_and_keep_detached_owner_alive() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<main><div id='owner' data-value='one'></div></main>",
            64,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"
                globalThis.retainedAttr = (() => {
                    const owner = document.getElementById('owner');
                    const map = owner.attributes;
                    const attr = owner.getAttributeNode('data-value');
                    const initial = map === owner.attributes && map instanceof NamedNodeMap &&
                        map.length === 2 && map.item(1) === attr &&
                        map.getNamedItem('data-value') === attr && attr instanceof Attr &&
                        attr instanceof Node && attr.nodeType === 2 &&
                        attr.name === 'data-value' && attr.ownerElement === owner;
                    attr.value = 'two';
                    const valueSetter = owner.getAttribute('data-value') === 'two';
                    owner.setAttribute('data-value', 'three');
                    const reflectedSetter = attr.value === 'three' &&
                        owner.getAttributeNode('data-value') === attr;
                    owner.removeAttribute('data-value');
                    const detached = map.length === 1 && attr.ownerElement === null &&
                        attr.value === 'three';
                    const attached = owner.setAttributeNode(attr) === null &&
                        map.length === 2 && attr.ownerElement === owner &&
                        map.getNamedItem('data-value') === attr;
                    owner.remove();
                    return initial && valueSetter && reflectedSetter && detached && attached
                        ? attr : null;
                })();
                retainedAttr instanceof Attr
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));

        // The Attr is the only JavaScript root for its detached Element here.
        // Reaping after collection must follow the owner’s materialized sidecar.
        engine.collect_garbage();
        realm.with_session(|_| ());
        let result = script(
            &mut engine,
            "retainedAttr.ownerElement.id === 'owner' && retainedAttr.ownerElement.parentNode === null && !retainedAttr.ownerElement.isConnected",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn html_reflected_attributes_use_null_namespace_and_options_are_live() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(
            engine.ctx(),
            "<main><select id='choices'><option id='first'>First</option><option id='second'>Second</option></select><input id='field'><details id='details'></details></main>",
            96,
        )
        .unwrap();
        let result = script(
            engine,
            r#"
                (() => {
                    const failures = [];
                    const check = (name, condition) => {
                        if (!condition) failures.push(name);
                    };
                    let phase = 'options collection setup';
                    try {
                    const select = document.getElementById('choices');
                    const options = select.options;
                    check('options is a same-object HTMLOptionsCollection',
                        options === select.options && options instanceof HTMLOptionsCollection &&
                        options instanceof HTMLCollection);
                    check('options initially reflects descendants',
                        options.length === 2 && options.item(0).id === 'first' &&
                        options[1].id === 'second');
                    phase = 'options insertion';
                    const added = document.createElement('option');
                    added.id = 'third';
                    select.appendChild(added);
                    check('options updates after insertion', options.length === 3 &&
                        options.item(2) === added);
                    phase = 'options selectedIndex';
                    options.selectedIndex = 2;
                    check('selectedIndex updates the owning select',
                        select.selectedIndex === 2 && options.selectedIndex === 2 &&
                        added.selected);
                    phase = 'options subtree pruning';
                    const datalist = document.createElement('datalist');
                    datalist.appendChild(document.createElement('option'));
                    select.appendChild(datalist);
                    const nestedSelect = document.createElement('select');
                    nestedSelect.appendChild(document.createElement('option'));
                    select.appendChild(nestedSelect);
                    const wrapper = document.createElement('div');
                    const wrappedOption = document.createElement('option');
                    wrapper.appendChild(wrappedOption);
                    select.appendChild(wrapper);
                    const parentOption = document.createElement('option');
                    parentOption.appendChild(document.createElement('option'));
                    select.appendChild(parentOption);
                    check('options use the HTML pruned descendant list', options.length === 5 &&
                        options[3] === wrappedOption && options[4] === parentOption);

                    phase = 'input reflected attributes';
                    const input = document.getElementById('field');
                    input.setAttributeNS('urn:lookalike', 'type', 'file');
                    check('namespaced type does not affect input.type', input.type === 'text');
                    input.type = 'email';
                    check('input.type writes the null-namespace attribute',
                        input.type === 'email' && input.getAttributeNS(null, 'type') === 'email' &&
                        input.getAttributeNS('urn:lookalike', 'type') === 'file');
                    input.setAttribute('TYPE', 'date');
                    check('generic setAttribute preserves first QName namespace',
                        input.getAttributeNS('urn:lookalike', 'type') === 'date' &&
                        input.type === 'email');
                    input.setAttribute('DaTa-Mode', 'compact');
                    check('HTML setAttribute names are ASCII-lowercased',
                        input.getAttribute('data-mode') === 'compact');

                    phase = 'details reflected attribute';
                    const details = document.getElementById('details');
                    details.setAttributeNS('urn:lookalike', 'open', '');
                    check('namespaced open does not affect details.open', !details.open);
                    details.open = true;
                    check('details.open creates null-namespace open', details.open &&
                        details.hasAttributeNS(null, 'open') &&
                        details.hasAttributeNS('urn:lookalike', 'open'));
                    details.open = false;
                    check('details.open removes only the reflected attribute', !details.open &&
                        !details.hasAttributeNS(null, 'open') &&
                        details.hasAttributeNS('urn:lookalike', 'open'));

                    phase = 'option reflected attribute';
                    const option = document.createElement('option');
                    option.setAttributeNS('urn:lookalike', 'selected', '');
                    check('namespaced selected does not affect reflected state',
                        !option.defaultSelected && !option.selected);
                    option.defaultSelected = true;
                    check('defaultSelected adds null-namespace selected',
                        option.defaultSelected && option.hasAttributeNS(null, 'selected') &&
                        option.hasAttributeNS('urn:lookalike', 'selected'));
                    option.defaultSelected = false;
                    check('defaultSelected removes only null-namespace selected',
                        !option.defaultSelected && option.hasAttributeNS('urn:lookalike', 'selected'));

                    phase = 'id and className reflection';
                    const identity = document.createElement('div');
                    identity.setAttributeNS('urn:lookalike', 'id', 'decorative');
                    identity.setAttributeNS('urn:lookalike', 'class', 'decorative');
                    check('id and className ignore namespaced lookalikes',
                        identity.id === '' && identity.className === '');
                    identity.id = 'actual';
                    identity.className = 'real';
                    check('id and className reflect null-namespace values',
                        identity.id === 'actual' && identity.className === 'real' &&
                        identity.getAttributeNS('urn:lookalike', 'id') === 'decorative' &&
                        identity.getAttributeNS('urn:lookalike', 'class') === 'decorative');

                    phase = 'attribute name validation';
                    const NativeDOMException = DOMException;
                    let invalidName;
                    try { input.setAttribute('bad name', 'x'); }
                    catch (error) { invalidName = error; }
                    check('setAttribute rejects whitespace in an attribute name',
                        invalidName instanceof NativeDOMException &&
                        invalidName.name === 'InvalidCharacterError' && invalidName.code === 5);
                    let invalidDelimiter;
                    try { input.toggleAttribute('a/b'); }
                    catch (error) { invalidDelimiter = error; }
                    check('toggleAttribute rejects DOM delimiters',
                        invalidDelimiter instanceof NativeDOMException &&
                        invalidDelimiter.name === 'InvalidCharacterError' && invalidDelimiter.code === 5);
                    const validDomNames = ['0', '0:a', ':', 'x:y:x', 'invalid^Name', '\\', "'", '"', '~'];
                    let validDomNamesWork = true;
                    for (let index = 0; index < validDomNames.length; index++) {
                        const name = validDomNames[index];
                        input.setAttribute(name, String(index));
                        validDomNamesWork = validDomNamesWork && input.getAttribute(name) === String(index);
                        input.removeAttribute(name);
                        validDomNamesWork = validDomNamesWork && input.toggleAttribute(name) &&
                            input.toggleAttribute(name) === false;
                    }
                    check('DOM attribute local names accept punctuation and digits', validDomNamesWork);
                    const localAttr = document.createAttribute('0');
                    let invalidAttrName;
                    try { document.createAttribute('a=b'); }
                    catch (error) { invalidAttrName = error; }
                    check('createAttribute uses the DOM local-name grammar',
                        localAttr.name === '0' &&
                        invalidAttrName instanceof NativeDOMException &&
                        invalidAttrName.name === 'InvalidCharacterError');
                    let invalidQName;
                    try { input.setAttributeNS('urn:lookalike', 'x:y:z', 'x'); }
                    catch (error) { invalidQName = error; }
                    check('setAttributeNS validates QNames',
                        invalidQName instanceof NativeDOMException &&
                        invalidQName.name === 'InvalidCharacterError' && invalidQName.code === 5);

                    phase = 'Attr namespace reflection';
                    const namespaced = document.createElement('foo');
                    namespaced.setAttributeNS('urn:example', 'p:token', 'one');
                    const attr = namespaced.attributes.item(0);
                    check('Attr exposes its exact namespace and QName parts',
                        attr.namespaceURI === 'urn:example' && attr.prefix === 'p' &&
                        attr.localName === 'token' && attr.name === 'p:token');
                    const xmlAttribute = document.createElement('foo');
                    xmlAttribute.setAttributeNS('http://www.w3.org/XML/1998/namespace', 'a:lang', 'en');
                    const xmlnsAttribute = document.createElement('foo');
                    xmlnsAttribute.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:a', 'urn:example');
                    check('XML and XMLNS Attr namespaces remain distinct',
                        xmlAttribute.attributes[0].namespaceURI ===
                            'http://www.w3.org/XML/1998/namespace' &&
                        xmlnsAttribute.attributes[0].namespaceURI ===
                            'http://www.w3.org/2000/xmlns/');
                    const duplicateName = document.createElement('foo');
                    duplicateName.setAttributeNS('urn:first', 'token', 'first');
                    duplicateName.setAttributeNS('urn:second', 'token', 'second');
                    check('toggleAttribute removes only the first QName match',
                        duplicateName.toggleAttribute('token') === false &&
                        duplicateName.attributes.length === 1 &&
                        duplicateName.attributes[0].namespaceURI === 'urn:second' &&
                        duplicateName.attributes[0].value === 'second');

                    phase = 'inline style assignment';
                    const styled = document.createElement('foo');
                    styled.style = 'color: red; background-color: green';
                    const liveStyle = styled.style;
                    check('style assignment forwards to the style declaration',
                        styled.hasAttributeNS(null, 'style') &&
                        liveStyle.getPropertyValue('color') !== '');
                    check('toggleAttribute removes an inline style attribute',
                        styled.toggleAttribute('style') === false &&
                        !styled.hasAttributeNS(null, 'style') && liveStyle.cssText === '');

                    phase = 'XML attribute semantics';
                    const xml = new DOMParser().parseFromString('<Root/>', 'application/xml');
                    const root = xml.documentElement;
                    root.setAttribute('CaseSensitive', 'one');
                    check('XML attribute names retain case',
                        root.getAttribute('CaseSensitive') === 'one' &&
                        root.getAttribute('casesensitive') === null);
                    check('toggleAttribute honors XML name case and force',
                        root.toggleAttribute('MiXeD') && root.hasAttribute('MiXeD') &&
                        !root.hasAttribute('mixed') && !root.toggleAttribute('MiXeD', false) &&
                        root.toggleAttribute('MiXeD', true));
                    return failures.join(', ');
                    } catch (error) {
                        const message = error && typeof error.message === 'string'
                            ? error.message : 'unknown thrown value';
                        return `exception during ${phase}: ${message}`;
                    }
                })()
            "#,
        );
        let Value::Str(failures) = result else {
            panic!("DOM regression did not return named diagnostics");
        };
        assert!(failures.is_empty(), "failed DOM assertions: {failures}");
    }

    #[test]
    fn node_replace_child_returns_old_node_and_splices_fragments() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(
            engine.ctx(),
            "<main><i id='old'></i><b id='tail'></b></main>",
            64,
        )
        .unwrap();
        let value = script(
            engine,
            r#"
            (() => {
                const main = document.querySelector('main');
                const old = document.getElementById('old');
                const tail = document.getElementById('tail');
                const replacement = document.createElement('span');
                const removed = main.replaceChild(replacement, old);
                const fragment = document.createDocumentFragment();
                fragment.append(document.createTextNode('x'), document.createElement('u'));
                const removedTail = main.replaceChild(fragment, tail);
                const foreignDocument = new DOMParser().parseFromString(
                    '<strong id="foreign">adopted</strong>', 'text/html');
                const foreign = foreignDocument.getElementById('foreign');
                const foreignFragment = foreignDocument.createDocumentFragment();
                foreignFragment.append(foreign);
                const removedReplacement = main.replaceChild(foreignFragment, replacement);
                const sameNode = main.replaceChild(foreign, foreign);
                let missing;
                try { main.replaceChild(document.createElement('em'), old); }
                catch (error) { missing = error; }
                return removed === old && removed.parentNode === null &&
                    removedTail === tail && tail.parentNode === null &&
                    removedReplacement === replacement && replacement.parentNode === null &&
                    sameNode === foreign && foreign.parentNode === main &&
                    foreign.ownerDocument === document &&
                    foreignFragment.ownerDocument === document && foreignFragment.childNodes.length === 0 &&
                    foreignDocument.getElementById('foreign') === null &&
                    fragment.childNodes.length === 0 && main.childNodes.length === 3 &&
                    main.childNodes[0] === foreign && main.childNodes[1].data === 'x' &&
                    main.childNodes[2].localName === 'u' &&
                    missing instanceof DOMException && missing.name === 'NotFoundError';
            })()
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn element_outer_html_replaces_in_order_and_updates_live_id_queries() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(
            engine.ctx(),
            "<main><section><b id='old'>A</b><i id='tail'>B</i></section><table><tbody id='old-body'><tr><td>old</td></tr></tbody></table></main>",
            96,
        )
        .unwrap();
        let value = script(
            engine,
            r#"
            (() => {
              const section = document.querySelector('section');
              const old = document.getElementById('old');
              const tail = document.getElementById('tail');
              old.outerHTML = '<em id="first">one</em><strong id="second">two</strong>';
              const first = document.getElementById('first');
              const second = document.getElementById('second');
              return section.childNodes.length === 3 &&
                section.childNodes[0] === first && section.childNodes[1] === second &&
                section.childNodes[2] === tail && old.parentNode === null &&
                old.textContent === 'A' && old.outerHTML === '<b id="old">A</b>' &&
                document.getElementById('old') === null &&
                section.innerHTML === '<em id="first">one</em>' +
                  '<strong id="second">two</strong><i id="tail">B</i>' &&
                (() => {
                  const table = document.querySelector('table');
                  const oldBody = document.getElementById('old-body');
                  oldBody.outerHTML = '<tr><td id="cell">new</td></tr>';
                  return oldBody.parentNode === null && table.firstElementChild.localName === 'tbody' &&
                    table.firstElementChild.firstElementChild.localName === 'tr' &&
                    table.querySelector('#cell').textContent === 'new' &&
                    document.createTextNode('plain').outerHTML === undefined;
                })();
            })()
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn outer_html_handles_fragment_context_detached_elements_and_custom_reactions() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let detached = script(
            engine,
            r#"
            (() => {
              const node = document.createElement('div');
              node.innerHTML = 'foo<p>bar</p>';
              node.outerHTML = '<p>ignored</p>';
              return node.outerHTML === '<div>foo<p>bar</p></div>' && node.parentNode === null;
            })()
            "#,
        );
        assert!(matches!(detached, Value::Bool(true)));

        let fragment = script(
            engine,
            r#"
            (() => {
              const fragment = document.createDocumentFragment();
              const old = document.createElement('div');
              old.innerHTML = 'foo<p>bar</p>';
              fragment.appendChild(old);
              const markup = '<div><h1>heading</h1><p>body</p></div>';
              old.outerHTML = `<body>${markup}</body>`;
              return old.parentNode === null && old.outerHTML === '<div>foo<p>bar</p></div>' &&
                fragment.firstChild.outerHTML === markup && fragment.childNodes.length === 1;
            })()
            "#,
        );
        assert!(matches!(fragment, Value::Bool(true)));

        let root_error = script(
            engine,
            r#"
            (() => {
              try { document.documentElement.outerHTML = '<html></html>'; }
              catch (error) {
                return error instanceof DOMException && error.name === 'NoModificationAllowedError';
              }
              return false;
            })()
            "#,
        );
        assert!(matches!(root_error, Value::Bool(true)));

        let xml = script(
            engine,
            r#"
            (() => {
              const xml = new DOMParser().parseFromString(
                '<root xmlns="urn:default" xmlns:p="urn:prefixed">' +
                  '<child>before</child></root>', 'application/xml');
              const child = xml.documentElement.firstChild;
              const before = child.outerHTML;
              let rejected = false;
              try { child.outerHTML = '<broken>'; }
              catch (error) {
                if (error.name !== 'SyntaxError' || xml.documentElement.firstChild !== child ||
                    child.textContent !== 'before') return false;
                rejected = true;
              }
              if (!rejected || child.outerHTML !== before) return false;
              child.outerHTML = '<leaf/><p:item>after</p:item>tail';
              const root = xml.documentElement;
              const leaf = root.firstChild;
              const item = leaf.nextSibling;
              const tail = item.nextSibling;
              return child.parentNode === null && before ===
                  '<child xmlns="urn:default">before</child>' &&
                leaf.localName === 'leaf' && leaf.namespaceURI === 'urn:default' &&
                item.localName === 'item' && item.namespaceURI === 'urn:prefixed' &&
                item.textContent === 'after' && tail.nodeType === Node.TEXT_NODE &&
                tail.data === 'tail' && xml.documentElement.outerHTML ===
                  '<root xmlns="urn:default" xmlns:p="urn:prefixed"><leaf/>' +
                    '<p:item>after</p:item>tail</root>';
            })()
            "#,
        );
        assert!(matches!(xml, Value::Bool(true)));

        let xml_void = script(
            engine,
            r#"
            (() => {
              const xml = new DOMParser().parseFromString(
                '<root xmlns:h="http://www.w3.org/1999/xhtml"><h:area/></root>',
                'application/xml');
              const area = xml.documentElement.firstChild;
              return area.outerHTML ===
                '<h:area xmlns:h="http://www.w3.org/1999/xhtml" />';
            })()
            "#,
        );
        assert!(matches!(xml_void, Value::Bool(true)));

        let xml_namespace_fixup = script(
            engine,
            r#"
            (() => {
              const xml = new DOMParser().parseFromString('<root/>', 'application/xml');
              const item = xml.createElementNS('urn:element', 'p:item');
              item.setAttributeNS('http://www.w3.org/2000/xmlns/', 'xmlns:p', 'urn:attribute');
              return item.outerHTML ===
                '<ns1:item xmlns:ns1="urn:element" xmlns:p="urn:attribute"/>';
            })()
            "#,
        );
        assert!(matches!(xml_namespace_fixup, Value::Bool(true)));

        let xml_well_formed = script(
            engine,
            r#"
            (() => {
              const xml = new DOMParser().parseFromString('<root/>', 'application/xml');
              const root = xml.documentElement;
              const invalid = [xml.createComment('bad--comment'), xml.createTextNode('\u0000')];
              for (const node of invalid) {
                root.appendChild(node);
                let rejected = false;
                try { root.outerHTML; }
                catch (error) { rejected = error.name === 'InvalidStateError'; }
                root.removeChild(node);
                if (!rejected) return false;
              }
              return root.outerHTML === '<root/>';
            })()
            "#,
        );
        assert!(matches!(xml_well_formed, Value::Bool(true)));

        script(
            engine,
            r#"
            var outerHTMLReactions = [];
            class BeforeOuterHTML extends HTMLElement {
              disconnectedCallback() { outerHTMLReactions.push('disconnected'); }
            }
            class AfterOuterHTML extends HTMLElement {
              constructor() { super(); outerHTMLReactions.push('constructed'); }
              connectedCallback() { outerHTMLReactions.push('connected'); }
            }
            customElements.define('before-outer-html', BeforeOuterHTML);
            customElements.define('after-outer-html', AfterOuterHTML);
            var outerHTMLOld = document.createElement('before-outer-html');
            document.querySelector('main').appendChild(outerHTMLOld);
            "#,
        );
        engine.ctx().drain_microtasks_for_host();
        script(
            engine,
            "outerHTMLReactions = []; outerHTMLOld.outerHTML = '<after-outer-html></after-outer-html>';",
        );
        engine.ctx().drain_microtasks_for_host();
        let reactions = script(
            engine,
            "outerHTMLReactions.join(',') === 'constructed,disconnected,connected'",
        );
        assert!(matches!(reactions, Value::Bool(true)));
    }

    #[test]
    fn window_proxy_forwards_runtime_and_lazy_web_globals() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let before = script(
            engine,
            r#"(() => ['Uint8Array', 'Map', 'WeakMap', 'TypeError']
                .filter(name => globalThis[name] !== ({Uint8Array, Map, WeakMap, TypeError})[name])
                .join(','))()"#,
        );
        match before {
            Value::Str(failures) => assert!(failures.is_empty(), "before install: {failures}"),
            _ => panic!("before-install global diagnostic did not return a string"),
        }

        install(engine.ctx(), "<main></main>", 64).unwrap();
        let after = script(
            engine,
            r#"(() => {
                const formData = FormData;
                const pairs = {Uint8Array, Map, WeakMap, TypeError, FormData: formData};
                return Object.keys(pairs)
                    .filter(name => globalThis[name] !== pairs[name])
                    .join(',');
            })()"#,
        );
        match after {
            Value::Str(failures) => assert!(failures.is_empty(), "after install: {failures}"),
            _ => panic!("after-install global diagnostic did not return a string"),
        }
    }

    #[test]
    fn window_proxy_forwards_pristine_lazy_url_and_decoder_globals() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let diagnostic = script(
            engine,
            r#"(() => {
                const failures = [];
                let parsed;
                try {
                    parsed = new URL('/x', 'https://example.test/');
                } catch (error) {
                    failures.push('URL initialization: ' + error.name + ': ' + error.message);
                }
                if (parsed && parsed.href !== 'https://example.test/x') failures.push('URL result');
                if (typeof URL === 'function' && globalThis.URL !== URL) failures.push('URL identity');
                if (typeof TextDecoder !== 'function') failures.push('TextDecoder missing');
                if (typeof TextDecoder === 'function' && globalThis.TextDecoder !== TextDecoder) {
                    failures.push('TextDecoder identity');
                }
                return failures.join('|');
            })()"#,
        );
        match diagnostic {
            Value::Str(failures) => assert!(failures.is_empty(), "lazy globals: {failures}"),
            _ => panic!("lazy-global diagnostic did not return a string"),
        }
    }

    #[test]
    fn node_constants_are_inherited_with_webidl_descriptors() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = script(
            &mut engine,
            r#"
            (() => {
                const text = document.createTextNode('text');
                if (text.nodeType !== Node.TEXT_NODE || text.TEXT_NODE !== 3 ||
                    Element.DOCUMENT_POSITION_CONTAINED_BY !== 16 ||
                    document.DOCUMENT_NODE !== 9) return false;
                for (const target of [Node, Node.prototype]) {
                    const d = Object.getOwnPropertyDescriptor(target, 'TEXT_NODE');
                    if (!d || d.value !== 3 || !d.enumerable || d.writable || d.configurable) return false;
                    if (Reflect.deleteProperty(target, 'TEXT_NODE')) return false;
                }
                return true;
            })()
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn empty_and_nomodule_scripts_preserve_preparation_lifecycle() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = script(
            &mut engine,
            r#"
            (() => {
                globalThis.preparationRuns = 0;
                const empty = document.createElement('script');
                document.body.appendChild(empty);
                empty.text = 'globalThis.preparationRuns++';
                if (preparationRuns !== 1) return false;
                const suppressed = document.createElement('script');
                suppressed.noModule = true;
                suppressed.text = 'globalThis.preparationRuns++';
                document.body.appendChild(suppressed);
                suppressed.remove();
                suppressed.noModule = false;
                document.body.appendChild(suppressed);
                if (preparationRuns !== 1) return false;
                const nested = document.createElement('script');
                const child = document.createElement('span');
                child.textContent = 'globalThis.preparationRuns++';
                nested.appendChild(child);
                if (nested.text !== '' || nested.textContent === '') return false;
                document.body.appendChild(nested);
                return preparationRuns === 1;
            })()
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn script_supports_reports_real_realm_capabilities_and_webidl_coercion() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.enable_resource_script_activations();
        assert!(matches!(script(&mut engine, r#"
            (() => {
                const supports = HTMLScriptElement.supports;
                const d = Object.getOwnPropertyDescriptor(HTMLScriptElement, 'supports');
                if (supports.length !== 1 || !d.enumerable || !d.writable || !d.configurable ||
                    'supports' in HTMLScriptElement.prototype || !supports('classic') ||
                    supports('module') || supports('importmap') || supports('speculationrules')) return false;
                for (const type of ['', ' ', 'Classic', 'classic ', ' classic', 'Module',
                    'module ', ' module', 'text/javascript', 'application/javascript', null, undefined]) {
                    if (supports(type)) return false;
                }
                if (!supports({toString() { return 'classic'; }})) return false;
                let missing = false, symbol = false;
                try { supports(); } catch (e) { missing = e instanceof TypeError; }
                try { supports(Symbol()); } catch (e) { symbol = e instanceof TypeError; }
                return missing && symbol;
            })()
        "#), Value::Bool(true)));
        realm.set_module_script_support(true);
        assert!(matches!(script(&mut engine, "HTMLScriptElement.supports('module')"), Value::Bool(true)));
        let parent_supports = script(&mut engine, "HTMLScriptElement.supports");
        let child = engine.ctx().create_host_realm();
        let (child_realm, child_supports) = engine.ctx().with_host_realm(&child, |ctx| {
            let child_realm = install(ctx, "<body></body>", 64).unwrap();
            assert!(!DomHtmlScriptElement::supports(ctx, "module"));
            child_realm.enable_resource_script_activations();
            assert!(!DomHtmlScriptElement::supports(ctx, "module"));
            let global = ctx.global_object();
            ctx.member_set(&global, "borrowedParentSupports", parent_supports).ok().expect("publish parent operation");
            let constructor = ctx.get_member(&global, "HTMLScriptElement").ok().expect("child constructor");
            let supports = ctx.get_member(&constructor, "supports").ok().expect("child operation");
            (child_realm, supports)
        }).unwrap();
        assert!(DomHtmlScriptElement::supports(engine.ctx(), "module"));
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, "borrowedChildSupports", child_supports).ok().expect("publish child operation");
        assert!(matches!(script(&mut engine, "borrowedChildSupports('module') === false"), Value::Bool(true)));
        let value = engine.eval_value_in_host_realm(&child,
            "borrowedParentSupports('module') === true && HTMLScriptElement.supports('module') === false", false)
            .unwrap().ok().expect("borrowed parent operation");
        assert!(matches!(value, Value::Bool(true)));
        child_realm.set_module_script_support(true);
        engine.ctx().with_host_realm(&child, |ctx| {
            assert!(DomHtmlScriptElement::supports(ctx, "module"));
        }).unwrap();
        realm.set_module_script_support(false);
        assert!(!DomHtmlScriptElement::supports(engine.ctx(), "module"));
    }

    #[test]
    fn script_type_parameters_and_legacy_language_control_preparation() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = script(
            &mut engine,
            r#"
            (() => {
                globalThis.scriptTypeRuns = 0;
                function append(type, language) {
                    const s = document.createElement('script');
                    if (type !== null) s.type = type;
                    if (language !== null) s.setAttribute('language', language);
                    s.text = 'globalThis.scriptTypeRuns++';
                    document.body.appendChild(s);
                }
                append('text/javascript;charset=UTF-8', null);
                append('text/javascript\u000b', null);
                append('   ', null);
                append(null, 'vbscript');
                if (scriptTypeRuns !== 0) return false;
                append(' \tTEXT/JAVASCRIPT\n', null);
                append(null, 'javascript1.2');
                append('', 'vbscript');
                return scriptTypeRuns === 3;
            })()
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn script_async_state_survives_attribute_changes_and_adoption() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<body><script id='parsed'></script></body>",
            64,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
            (() => {
                const parsed = document.getElementById('parsed');
                const dynamic = document.createElement('script');
                if (parsed.async || !dynamic.async || dynamic.hasAttribute('async')) return false;
                dynamic.setAttribute('async', '');
                dynamic.removeAttribute('async');
                if (dynamic.async) return false;
                dynamic.async = true;
                if (!dynamic.hasAttribute('async')) return false;
                dynamic.async = false;
                if (dynamic.hasAttribute('async') || dynamic.async) return false;
                dynamic.defer = true;
                dynamic.noModule = true;
                if (!dynamic.hasAttribute('defer') || !dynamic.hasAttribute('nomodule')) return false;
                const other = document.implementation.createHTMLDocument('other');
                other.adoptNode(dynamic);
                if (dynamic.async || !dynamic.defer || !dynamic.noModule) return false;
                dynamic.defer = false;
                dynamic.noModule = false;
                return !dynamic.hasAttribute('defer') && !dynamic.hasAttribute('nomodule');
            })()
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn dynamic_classic_script_runs_synchronously_once_with_current_script_identity() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        let value = script(
            &mut engine,
            r#"
                const dynamic = document.createElement('script');
                dynamic.type = 'text/javascript';
                dynamic.text = "globalThis.dynamicRuns = (globalThis.dynamicRuns || 0) + 1; globalThis.currentScriptMatches = document.currentScript === globalThis.dynamicNode;";
                globalThis.dynamicNode = dynamic;
                document.body.appendChild(dynamic);
                const synchronous = dynamicRuns === 1 && currentScriptMatches && document.currentScript === null;
                dynamic.remove();
                document.body.appendChild(dynamic);
                synchronous && dynamicRuns === 1 && currentScriptMatches
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        let node = realm.with_session(|session| {
            selector::query_selector(session.document(), session.document().root(), "script")
                .unwrap()
                .unwrap()
        });
        assert!(realm.script_started(node));
        assert!(realm.script_is_classic(node));
    }

    #[test]
    fn parser_script_type_change_and_child_removal_do_not_prepare_it() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<body><script id=parser type=application/json>globalThis.parserRuns = 1;</script></body>",
            64,
        )
        .unwrap();
        let value = script(
            &mut engine,
            "const parserScript=document.getElementById('parser'); parserScript.type=''; parserScript.removeChild(parserScript.firstChild); globalThis.parserRuns === undefined",
        );
        assert!(matches!(value, Value::Bool(true)));
        let node = realm.with_session(|session| {
            selector::query_selector(session.document(), session.document().root(), "#parser")
                .unwrap()
                .unwrap()
        });
        assert!(!realm.script_started(node));
    }

    #[test]
    fn parser_created_scripts_stay_inert_after_adoption_and_template_extraction() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<body><template id=source><script id=template-script>globalThis.inertRuns++;</script></template></body>",
            128,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
                globalThis.inertRuns = 0;
                const parsed = new DOMParser().parseFromString(
                    '<script id="parsed-script">globalThis.inertRuns++;</script>',
                    'text/html');
                const parsedScript = parsed.querySelector('script');
                document.body.appendChild(document.adoptNode(parsedScript));
                const template = document.getElementById('source');
                const templateScript = template.content.firstChild;
                document.body.appendChild(template.content.removeChild(templateScript));
                inertRuns === 0
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        let (parsed, template) = realm.with_session(|session| {
            let document = session.document();
            (
                selector::query_selector(document, document.root(), "#parsed-script")
                    .unwrap()
                    .unwrap(),
                selector::query_selector(document, document.root(), "#template-script")
                    .unwrap()
                    .unwrap(),
            )
        });
        assert!(realm.script_started(parsed));
        assert!(realm.script_started(template));
    }

    #[test]
    fn clone_started_state_inner_html_inertness_and_unhandled_module_are_native() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 128).unwrap();
        let value = script(
            &mut engine,
            r#"
                globalThis.cloneRuns = 0;
                const original = document.createElement('script');
                original.text = 'globalThis.cloneRuns++;';
                const unstartedClone = original.cloneNode(true);
                document.body.appendChild(unstartedClone);
                document.body.appendChild(original);
                document.body.appendChild(original);
                const startedClone = original.cloneNode(true);
                document.body.appendChild(startedClone);
                const host = document.createElement('div');
                host.innerHTML = '<script>globalThis.cloneRuns += 100;</script>';
                document.body.appendChild(host);
                const module = document.createElement('script');
                module.id = 'unhandled-module';
                module.type = 'module';
                module.text = 'globalThis.cloneRuns += 1000;';
                document.body.appendChild(module);
                cloneRuns === 2
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        let module = realm.with_session(|session| {
            selector::query_selector(
                session.document(),
                session.document().root(),
                "#unhandled-module",
            )
            .unwrap()
            .unwrap()
        });
        assert!(realm.script_started(module));
        assert_eq!(
            realm.unhandled_script_activations(),
            vec![UnhandledScriptActivation {
                node: module,
                reason: UnhandledScriptReason::Module,
                src: None,
            }]
        );
    }

    #[test]
    fn document_clone_preserves_type_metadata_and_detached_clone_state() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<!doctype html><html><head></head><body><noscript id=active-noscript><span>fallback</span></noscript></body></html>",
            512,
        )
        .unwrap();
        realm.set_document_url("https://clone.example.test/page.html");
        let value = script(
            &mut engine,
            r#"
                globalThis.documentCloneRuns = 0;
                const activeShallow = document.cloneNode(0);
                const activeClone = document.cloneNode(1);
                const activeNoScript = document.querySelector('#active-noscript');
                const cloneNoScript = activeClone.querySelector('#active-noscript');
                const detached = document.implementation.createHTMLDocument('copy');
                const input = detached.createElement('input');
                input.id = 'control';
                input.setAttribute('type', 'range');
                input.setAttribute('min', '0');
                input.setAttribute('max', '10');
                input.setAttribute('value', '2');
                detached.body.appendChild(input);
                input.value = '7';
                const link = detached.createElement('link');
                link.rel = 'stylesheet';
                link.href = 'https://clone.example.test/external.css';
                detached.head.appendChild(link);
                const dormantScript = detached.createElement('script');
                dormantScript.text = 'globalThis.documentCloneRuns++;';
                detached.body.appendChild(dormantScript);

                const htmlClone = detached.cloneNode(1);
                const htmlShallow = detached.cloneNode(0);
                const cloneInput = htmlClone.getElementById('control');
                const cloneLink = htmlClone.querySelector('link');
                const cloneScript = htmlClone.querySelector('script');
                document.body.appendChild(cloneScript);

                const doctype = document.implementation.createDocumentType(
                    'root', 'public-id', 'system-id');
                const xml = document.implementation.createDocument(
                    'urn:clone', 'root', doctype);
                const xmlClone = xml.cloneNode(1);
                const xmlShallow = xml.cloneNode(0);
                const check = (label, condition) => {
                    if (!condition) throw new Error('document clone: ' + label);
                };
                check('active shallow identity', activeShallow !== document);
                check('active shallow detached state', activeShallow.defaultView === null &&
                    activeShallow.location === null && activeShallow.documentElement === null);
                check('active shallow URL', activeShallow.URL === document.URL);
                check('active noscript mode', activeNoScript.textContent === '<span>fallback</span>' &&
                    getComputedStyle(activeNoScript).display === 'none');
                check('detached clone noscript mode and copied source',
                    cloneNoScript.textContent === '<span>fallback</span>' &&
                    getComputedStyle(cloneNoScript).display !== 'none');
                check('HTML clone interface', htmlClone !== detached &&
                    htmlClone instanceof Document && !(htmlClone instanceof XMLDocument));
                check('HTML clone MIME and URL', htmlClone.contentType === 'text/html' &&
                    htmlClone.URL === detached.URL);
                check('HTML clone detached state', htmlClone.defaultView === null &&
                    htmlClone.location === null);
                check('HTML clone doctype and children', htmlClone.doctype.name === 'html' &&
                    htmlClone.childNodes.length === 2);
                check('HTML clone independent body', htmlClone.body !== detached.body);
                check('HTML clone live range value', cloneInput.value === '7');
                check('HTML clone range default attribute',
                    cloneInput.getAttribute('value') === '2');
                check('HTML clone stylesheet link', cloneLink !== link &&
                    cloneLink.getAttribute('href') === link.getAttribute('href'));
                check('HTML clone script identity', cloneScript !== dormantScript);
                check('HTML clone script activation', documentCloneRuns === 1);
                check('HTML shallow clone', htmlShallow.childNodes.length === 0 &&
                    htmlShallow.documentElement === null);
                check('XML clone interface and MIME',
                    xmlClone.constructor === XMLDocument &&
                    xmlClone.contentType === 'application/xml');
                check('XML clone URL and detached state', xmlClone.URL === xml.URL &&
                    xmlClone.defaultView === null);
                check('XML clone child count', xmlClone.childNodes.length === 2);
                check('XML clone doctype identifiers', xmlClone.doctype.name === 'root' &&
                    xmlClone.doctype.publicId === 'public-id' &&
                    xmlClone.doctype.systemId === 'system-id');
                check('XML clone namespace',
                    xmlClone.documentElement.namespaceURI === 'urn:clone');
                check('XML shallow clone interface',
                    xmlShallow.constructor === XMLDocument && xmlShallow.childNodes.length === 0);
                true
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        assert_eq!(
            realm.document_url().as_deref(),
            Some("https://clone.example.test/page.html")
        );
    }

    #[test]
    fn shallow_clone_and_import_copy_clonable_shadow_roots_and_started_scripts() {
        let mut engine = Engine::new();
        let _realm = install(
            engine.ctx(),
            "<body><div id=host><template shadowrootmode=open shadowrootclonable></template></div></body>",
            128,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
                globalThis.shadowCloneRuns = 0;
                const host = document.getElementById('host');
                const original = document.createElement('script');
                original.text = 'globalThis.shadowCloneRuns++;';
                host.shadowRoot.appendChild(original);
                const shallow = host.cloneNode(false);
                const imported = document.importNode(host, false);
                document.body.appendChild(shallow);
                document.body.appendChild(imported);
                const shallowScript = shallow.shadowRoot.firstChild;
                const importedScript = imported.shadowRoot.firstChild;
                shallow.shadowRoot.removeChild(shallowScript);
                shallow.shadowRoot.appendChild(shallowScript);
                imported.shadowRoot.removeChild(importedScript);
                imported.shadowRoot.appendChild(importedScript);
                shallow.shadowRoot instanceof ShadowRoot &&
                    imported.shadowRoot instanceof ShadowRoot &&
                    shallowScript !== original &&
                    importedScript !== original &&
                    shallowScript.text === original.text &&
                    importedScript.text === original.text &&
                    shadowCloneRuns === 1
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn image_base_changes_preserve_selected_requests_until_source_update() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<base href='/first/'><body></body>", 64).unwrap();
        realm.set_document_url("https://images.test/page.html");
        let calls = Rc::new(RefCell::new(Vec::<String>::new()));
        let recorded = calls.clone();
        let bitmap = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![0; 4],
        });
        realm.set_image_resolver(Rc::new(move |source: &str| {
            recorded.borrow_mut().push(source.to_owned());
            lumen_html::layout::ImageState::Ready(bitmap.clone())
        }));
        script(
            &mut engine,
            "globalThis.image = new Image(); globalThis.loads = 0; image.onload = () => loads++; image.src = 'pixel.png';",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        script(
            &mut engine,
            "document.querySelector('base').href = '/second/';",
        );
        assert!(matches!(
            script(
                &mut engine,
                "image.complete && image.currentSrc === 'https://images.test/first/pixel.png' && image.src === 'https://images.test/second/pixel.png'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "loads === 1"),
            Value::Bool(true)
        ));
        assert_eq!(
            calls.borrow().as_slice(),
            &["https://images.test/first/pixel.png"]
        );

        // An identical relative attribute assignment is a new source update.
        script(&mut engine, "image.src = 'pixel.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(
                &mut engine,
                "loads === 2 && image.currentSrc === 'https://images.test/second/pixel.png'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(
            calls.borrow().as_slice(),
            &[
                "https://images.test/first/pixel.png",
                "https://images.test/second/pixel.png"
            ]
        );

        realm.set_document_url("https://images.test/history/new.html");
        script(&mut engine, "document.querySelector('base').remove();");
        assert!(matches!(
            script(
                &mut engine,
                "image.complete && image.currentSrc === 'https://images.test/second/pixel.png' && image.src === 'https://images.test/history/pixel.png'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert_eq!(calls.borrow().len(), 2);
    }

    #[test]
    fn image_same_source_updates_reuse_current_and_pending_requests() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("https://images.test/page.html");
        let calls = Rc::new(RefCell::new(Vec::<String>::new()));
        let recorded = calls.clone();
        let pending_ready = Rc::new(Cell::new(false));
        let ready = pending_ready.clone();
        let bitmap = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        });
        let decoded = bitmap.clone();
        realm.set_image_resolver(Rc::new(move |source: &str| {
            recorded.borrow_mut().push(source.to_owned());
            if source.ends_with("/pending.png") && !ready.get() {
                lumen_html::layout::ImageState::Pending
            } else {
                lumen_html::layout::ImageState::Ready(decoded.clone())
            }
        }));

        script(
            &mut engine,
            "globalThis.image = new Image(); globalThis.loads = 0; image.onload = () => loads++; image.src = '/pixel.png';",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);

        // A same-selection source mutation supersedes the queued task, but it
        // can use the already decoded current image and queue a fresh load
        // event without resolving or decoding the URL again.
        script(&mut engine, "image.src = '/pixel.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert_eq!(calls.borrow().len(), 1);
        assert_eq!(calls.borrow()[0], "https://images.test/pixel.png");
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "loads === 1"),
            Value::Bool(true)
        ));

        // A later same-value update after dispatch likewise uses the current
        // available image, but still performs the relevant load-event steps.
        script(&mut engine, "image.src = '/pixel.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert_eq!(calls.borrow().len(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "loads === 2"),
            Value::Bool(true)
        ));

        // setAttribute with an unchanged value follows the same content-attribute
        // mutation path and queues a fresh load event without a resolver call.
        script(&mut engine, "image.setAttribute('src', '/pixel.png');");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert_eq!(calls.borrow().len(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "loads === 3"),
            Value::Bool(true)
        ));

        // Repeating the source while a replacement is pending preserves that
        // request instead of replacing it. Its eventual completion owns the
        // sole load event for the current selection.
        script(&mut engine, "image.src = '/pending.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        script(&mut engine, "image.src = '/pending.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert!(matches!(
            script(
                &mut engine,
                "!image.complete && image.currentSrc === 'https://images.test/pixel.png'"
            ),
            Value::Bool(true)
        ));
        pending_ready.set(true);
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(matches!(
            script(
                &mut engine,
                "loads === 3 && image.complete && image.currentSrc === 'https://images.test/pending.png'"
            ),
            Value::Bool(true)
        ));
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(
                &mut engine,
                "loads === 4 && image.complete && image.currentSrc === 'https://images.test/pending.png'"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn image_crossorigin_mutation_resolves_base_only_when_state_changes() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<base href='/first/'><body><img crossorigin='anonymous' src='pixel.png'></body>",
            64,
        )
        .unwrap();
        realm.set_document_url("https://images.test/page.html");
        let calls = Rc::new(Cell::new(0usize));
        let count = calls.clone();
        let replacement_ready = Rc::new(Cell::new(false));
        let ready = replacement_ready.clone();
        let bitmap = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 255],
        });
        realm.set_image_resolver(Rc::new(move |source: &str| {
            count.set(count.get() + 1);
            if source.ends_with("/second/pixel.png") && !ready.get() {
                lumen_html::layout::ImageState::Pending
            } else {
                lumen_html::layout::ImageState::Ready(bitmap.clone())
            }
        }));

        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        script(
            &mut engine,
            "document.querySelector('base').href = '/second/'; document.querySelector('img').setAttribute('crossorigin', 'ANONYMOUS');",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert!(matches!(
            script(
                &mut engine,
                "document.querySelector('img').currentSrc === 'https://images.test/first/pixel.png'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(calls.get(), 1);

        script(
            &mut engine,
            "document.querySelector('img').removeAttribute('crossorigin');",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert_eq!(calls.get(), 2);
        assert!(matches!(
            script(
                &mut engine,
                "!document.querySelector('img').complete && document.querySelector('img').currentSrc === 'https://images.test/first/pixel.png'"
            ),
            Value::Bool(true)
        ));
        replacement_ready.set(true);
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert_eq!(calls.get(), 3);
        assert!(matches!(
            script(
                &mut engine,
                "document.querySelector('img').complete && document.querySelector('img').currentSrc === 'https://images.test/second/pixel.png'"
            ),
            Value::Bool(true)
        ));
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
    }

    #[test]
    fn image_requests_expose_intrinsic_state_and_queue_stale_safe_events() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("https://images.test/page.html");
        let resolver_calls = Rc::new(Cell::new(0usize));
        let decoded = Arc::new(lumen_html::paint::ImageData {
            width: 2,
            height: 3,
            pixels: vec![0; 2 * 3 * 4],
        });
        let ready = decoded.clone();
        let calls = resolver_calls.clone();
        realm.set_image_resolver(Rc::new(move |source: &str| {
            calls.set(calls.get() + 1);
            if source.ends_with("/ok.png") {
                lumen_html::layout::ImageState::Ready(ready.clone())
            } else {
                lumen_html::layout::ImageState::Failed
            }
        }));

        assert!(matches!(
            script(
                &mut engine,
                "globalThis.image = new Image(4, 5); globalThis.imageEvents = []; image.onload = () => imageEvents.push('load'); image.onerror = () => imageEvents.push('error'); image instanceof HTMLImageElement && image instanceof HTMLElement && image.width === 4 && image.height === 5 && image.complete && image.naturalWidth === 0 && (image.src = 'ok.png', !image.complete && image.currentSrc === 'https://images.test/ok.png')"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(matches!(
            script(
                &mut engine,
                "image.complete && image.currentSrc === 'https://images.test/ok.png' && image.naturalWidth === 2 && image.naturalHeight === 3 && imageEvents.length === 0"
            ),
            Value::Bool(true)
        ));
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "imageEvents.join(',') === 'load'"),
            Value::Bool(true)
        ));

        assert!(matches!(
            script(
                &mut engine,
                "image.src = 'bad.png'; !image.complete && image.naturalWidth === 2 && image.currentSrc === 'https://images.test/ok.png'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(matches!(
            script(
                &mut engine,
                "image.complete && image.naturalWidth === 0 && image.currentSrc === 'https://images.test/bad.png' && imageEvents.join(',') === 'load'"
            ),
            Value::Bool(true)
        ));
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(
                &mut engine,
                "imageEvents.join(',') === 'load,error' && image.complete && image.naturalWidth === 0"
            ),
            Value::Bool(true)
        ));

        script(&mut engine, "image.src = 'ok.png'");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(matches!(
            script(
                &mut engine,
                "image.complete && image.naturalWidth === 2 && image.naturalHeight === 3 && image.width === 4 && image.height === 5 && imageEvents.join(',') === 'load,error'"
            ),
            Value::Bool(true)
        ));
        // Changing the source after completion was resolved but before its
        // queued event runs invalidates that event by request generation.
        script(&mut engine, "image.src = 'bad.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "imageEvents.join(',') === 'load,error,error'"),
            Value::Bool(true)
        ));

        assert!(matches!(
            script(
                &mut engine,
                "globalThis.emptyImage = new Image(); globalThis.emptyErrors = 0; emptyImage.onerror = () => emptyErrors++; emptyImage.complete && (emptyImage.src = '', emptyImage.complete && emptyImage.currentSrc === '')"
            ),
            Value::Bool(true)
        ));
        let calls_before_empty = resolver_calls.get();
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert_eq!(resolver_calls.get(), calls_before_empty);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(
                &mut engine,
                "emptyImage.complete && emptyErrors === 1 && new Image().complete"
            ),
            Value::Bool(true)
        ));

        script(
            &mut engine,
            "globalThis.detachedImageEvents = 0; globalThis.detachedImage = new Image(); detachedImage.onload = () => detachedImageEvents++; detachedImage.src = 'ok.png';",
        );
        script(&mut engine, "detachedImage = null");
        engine.collect_garbage();
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        engine.collect_garbage();
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "detachedImageEvents === 1"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn image_pending_replacement_keeps_current_bitmap_then_publishes_decoded_pixels() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<body><img style='display:block;width:2px;height:2px' src='red.png'></body>",
            64,
        )
        .unwrap();
        realm.set_document_url("https://images.test/page.html");
        let red = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![255, 0, 0, 255],
        });
        let blue = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 255, 255],
        });
        let pending_ready = Rc::new(Cell::new(false));
        let ready_flag = pending_ready.clone();
        let resolver_red = red.clone();
        let resolver_blue = blue.clone();
        realm.set_image_resolver(Rc::new(move |source: &str| {
            if source.ends_with("/red.png") {
                lumen_html::layout::ImageState::Ready(resolver_red.clone())
            } else if source.ends_with("/pending.png") && !ready_flag.get() {
                lumen_html::layout::ImageState::Pending
            } else if source.ends_with("/pending.png") || source.ends_with("/blue.png") {
                lumen_html::layout::ImageState::Ready(resolver_blue.clone())
            } else {
                lumen_html::layout::ImageState::Failed
            }
        }));

        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let rendered_pixels = |realm: &Rc<DomRealm>| {
            realm.with_session(|session| {
                render_with_font(
                    session.display_list(32, 32, &font).unwrap(),
                    32,
                    32,
                    1.0,
                    false,
                    &font,
                )
                .unwrap()
                .pixels
            })
        };
        let first = rendered_pixels(&realm);
        assert!(first
            .chunks_exact(4)
            .any(|pixel| pixel == &[255, 0, 0, 255]));
        let same_red_arc = realm.with_session(|session| {
            session
                .display_list(32, 32, &font)
                .unwrap()
                .0
                .iter()
                .any(|command| matches!(command, lumen_html::paint::Command::Image { image, .. } if Arc::ptr_eq(image, &red)))
        });
        assert!(same_red_arc);

        assert!(matches!(
            script(
                &mut engine,
                "globalThis.pixelImage = document.querySelector('img'); pixelImage.src = 'pending.png'; !pixelImage.complete && pixelImage.currentSrc === 'https://images.test/red.png' && pixelImage.naturalWidth === 1"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        let while_pending = rendered_pixels(&realm);
        assert!(while_pending
            .chunks_exact(4)
            .any(|pixel| pixel == &[255, 0, 0, 255]));
        assert!(!while_pending
            .chunks_exact(4)
            .any(|pixel| pixel == &[0, 0, 255, 255]));

        script(
            &mut engine,
            "globalThis.pendingImageEvents = 0; globalThis.pendingImage = new Image(); pendingImage.onload = () => pendingImageEvents++; pendingImage.src = 'pending.png';",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        script(&mut engine, "pendingImage = null");
        engine.collect_garbage();

        pending_ready.set(true);
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 2);
        assert!(matches!(
            script(
                &mut engine,
                "pixelImage.complete && pixelImage.currentSrc === 'https://images.test/pending.png' && pixelImage.naturalWidth === 1"
            ),
            Value::Bool(true)
        ));
        let second = rendered_pixels(&realm);
        assert!(second
            .chunks_exact(4)
            .any(|pixel| pixel == &[0, 0, 255, 255]));
        assert!(!second
            .chunks_exact(4)
            .any(|pixel| pixel == &[255, 0, 0, 255]));
        let same_blue_arc = realm.with_session(|session| {
            session
                .display_list(32, 32, &font)
                .unwrap()
                .0
                .iter()
                .any(|command| matches!(command, lumen_html::paint::Command::Image { image, .. } if Arc::ptr_eq(image, &blue)))
        });
        assert!(same_blue_arc);
        assert!(scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            script(&mut engine, "pendingImageEvents === 1"),
            Value::Bool(true)
        ));

        script(&mut engine, "pixelImage.removeAttribute('src')");
        assert!(matches!(
            script(
                &mut engine,
                "pixelImage.complete && pixelImage.currentSrc === '' && pixelImage.naturalWidth === 0"
            ),
            Value::Bool(true)
        ));
        let cleared = rendered_pixels(&realm);
        assert!(!cleared
            .chunks_exact(4)
            .any(|pixel| pixel == &[0, 0, 255, 255]));
        assert!(!cleared
            .chunks_exact(4)
            .any(|pixel| pixel == &[255, 0, 0, 255]));
    }

    #[test]
    fn canvas_draw_image_uses_decoded_html_image_pixels_and_rejects_unavailable_sources() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 128).unwrap();
        realm.set_document_url("https://images.test/page.html");
        let decoded = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![20, 40, 200, 255],
        });
        let ready = decoded.clone();
        let replacement = Arc::new(lumen_html::paint::ImageData {
            width: 1,
            height: 1,
            pixels: vec![190, 60, 10, 255],
        });
        let pending_ready = Rc::new(Cell::new(false));
        let ready_flag = pending_ready.clone();
        let replacement_ready = replacement.clone();
        realm.set_image_resolver(Rc::new(move |source: &str| {
            if source.ends_with("/ok.png") {
                lumen_html::layout::ImageState::Ready(ready.clone())
            } else if source.ends_with("/pending.png") && !ready_flag.get() {
                lumen_html::layout::ImageState::Pending
            } else if source.ends_with("/pending.png") {
                lumen_html::layout::ImageState::Ready(replacement_ready.clone())
            } else {
                lumen_html::layout::ImageState::Failed
            }
        }));
        script(
            &mut engine,
            "globalThis.loadedImage = new Image(); loadedImage.src = 'ok.png'; document.body.appendChild(loadedImage); globalThis.brokenImage = new Image(); brokenImage.src = 'broken.png'; document.body.appendChild(brokenImage); globalThis.pendingImage = new Image(); pendingImage.src = 'pending.png'; document.body.appendChild(pendingImage);",
        );
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 2);
        let value = script(
            &mut engine,
            r#"
                (() => {
                    globalThis.imageCanvas = document.createElement('canvas');
                    imageCanvas.width = 1;
                    imageCanvas.height = 1;
                    globalThis.imageContext = imageCanvas.getContext('2d');
                    const failures = [];
                    try {
                        imageContext.drawImage(loadedImage, 0, 0);
                        const pixel = Array.from(imageContext.getImageData(0, 0, 1, 1).data);
                        if (pixel.join(',') !== '20,40,200,255') failures.push(`decoded image pixel was ${pixel.join(',')}`);
                    } catch (error) {
                        failures.push(`decoded image draw threw ${error.name}: ${error.message}`);
                    }
                    try { imageContext.drawImage(new Image(), 0, 0); }
                    catch (error) { failures.push(`source-less image threw ${error.name}`); }
                    try { imageContext.drawImage(pendingImage, 0, 0); }
                    catch (error) { failures.push(`pending image threw ${error.name}`); }
                    const unchanged = Array.from(imageContext.getImageData(0, 0, 1, 1).data).join(',');
                    if (unchanged !== '20,40,200,255') failures.push(`unavailable image changed canvas to ${unchanged}`);
                    if (imageContext.createPattern(pendingImage) !== null) failures.push('pending pattern was not null');
                    let patternError;
                    try { imageContext.createPattern(brokenImage); }
                    catch (error) { patternError = error.name; }
                    if (patternError !== 'InvalidStateError') failures.push(`broken pattern threw ${patternError}`);
                    let brokenDrawError;
                    try { imageContext.drawImage(brokenImage, 0, 0); }
                    catch (error) { brokenDrawError = error.name; }
                    if (brokenDrawError !== 'InvalidStateError') failures.push(`broken image draw threw ${brokenDrawError}`);
                    globalThis.bitmapStates = [];
                    createImageBitmap(pendingImage).then(() => bitmapStates.push('pending:resolved'), error => bitmapStates.push(`pending:${error.name}`));
                    createImageBitmap(brokenImage).then(() => bitmapStates.push('broken:resolved'), error => bitmapStates.push(`broken:${error.name}`));
                    return failures.join(' | ');
                })()
            "#,
        );
        match value {
            Value::Str(failures) => assert!(failures.as_str().is_empty(), "{failures}"),
            _ => panic!("canvas image regression did not return diagnostics"),
        }
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "bitmapStates.sort().join(',') === 'broken:InvalidStateError,pending:InvalidStateError'"
            ),
            Value::Bool(true)
        ));
        script(&mut engine, "loadedImage.src = 'pending.png';");
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 0);
        assert!(matches!(
            script(
                &mut engine,
                "!loadedImage.complete && (() => { imageContext.drawImage(loadedImage, 0, 0); const pixel = imageContext.getImageData(0, 0, 1, 1).data; return pixel[0] === 20 && pixel[1] === 40 && pixel[2] === 200 && pixel[3] === 255; })()"
            ),
            Value::Bool(true)
        ));
        pending_ready.set(true);
        // Both the separate pending source and the replacement become ready.
        assert_eq!(realm.queue_image_tasks(engine.ctx()).unwrap(), 2);
        assert!(matches!(
            script(
                &mut engine,
                "pendingImage.complete && pendingImage.naturalWidth === 1 && loadedImage.complete && (() => { imageContext.drawImage(loadedImage, 0, 0); const pixel = imageContext.getImageData(0, 0, 1, 1).data; return pixel[0] === 190 && pixel[1] === 60 && pixel[2] === 10 && pixel[3] === 255; })()"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn collections_are_live_indexed_iterable_and_validate_all_tokens() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><b>A</b>text<i>B</i></main>", 64).unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const nodes = main.childNodes; const elements = main.children; const live = nodes instanceof NodeList && elements instanceof HTMLCollection && nodes.length === 3 && elements.length === 2 && nodes[0] === main.firstChild && elements[1] === main.lastChild && ('1' in nodes) && !('3' in nodes) && Object.keys(nodes).join(',') === '0,1,2' && Object.getOwnPropertyDescriptor(nodes, '0').value === main.firstChild && [...nodes].length === 3; main.appendChild(document.createElement('p')); main.classList.add('a', 'b'); let rejected = false; try { main.classList.add('ok', 'bad token'); } catch(e) { rejected = true; } live && nodes === main.childNodes && nodes.length === 4 && elements.length === 3 && rejected && !main.classList.contains('ok') && main.classList[1] === 'b' && [...main.classList].join(',') === 'a,b' && nodes[99] === undefined && nodes.item(99) === null").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn descendant_collections_coerce_dom_strings_and_back_typed_keywords_to_computed_style() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><div id=target></div></main>", 128).unwrap();
        let value = script(
            &mut engine,
            r#"
                const main = document.querySelector('main');
                const target = document.getElementById({toString() { return 'target'; }});
                const docSpans = document.getElementsByTagName('span');
                const elementSpans = main.getElementsByTagName('span');
                const docClasses = document.getElementsByClassName('matched');
                const nullNamespace = document.getElementsByTagNameNS(null, 'x');
                const stringNamespace = document.getElementsByTagNameNS('null', 'x');
                const nullNode = document.createElementNS('', 'x');
                const stringNode = document.createElementNS('null', 'x');
                main.append(nullNode, stringNode);
                const span = document.createElement('span');
                span.className = 'matched';
                target.appendChild(span);
                const before = docSpans === document.getElementsByTagName('span') &&
                    elementSpans === main.getElementsByTagName('span') &&
                    docSpans.length === 1 && elementSpans.length === 1 &&
                    docClasses.length === 1 && nullNamespace !== stringNamespace &&
                    nullNamespace.length === 1 && stringNamespace.length === 1 &&
                    nullNamespace[0] === nullNode && stringNamespace[0] === stringNode;
                span.className = 'other';
                const classListStayedLive = docClasses.length === 0;
                span.setAttribute('class', 'matched other');
                const stringified = span.setAttribute('data-value', {toString() { return 'coerced'; }}) === undefined &&
                    span.getAttribute('data-value') === 'coerced';
                const thrownValue = {};
                let conversionThrew = false;
                try {
                    span.setAttribute('data-bad', {toString() { throw thrownValue; }});
                } catch (error) {
                    conversionThrew = error === thrownValue;
                }
                const styleMap = target.attributeStyleMap;
                styleMap.set('display', new CSSKeywordValue('inline'));
                const keyword = styleMap.get('display');
                const parsed = CSSStyleValue.parse('display', 'block');
                const parsedAll = CSSStyleValue.parseAll('display', 'block');
                let emptyKeywordThrew = false;
                try {
                    new CSSKeywordValue('');
                } catch (error) {
                    emptyKeywordThrew = error instanceof TypeError;
                }
                before && classListStayedLive && docClasses.length === 1 &&
                    stringified && conversionThrew && emptyKeywordThrew &&
                    styleMap === target.attributeStyleMap &&
                    keyword instanceof CSSKeywordValue && keyword.value === 'inline' &&
                    keyword.toString() === 'inline' && parsed instanceof CSSKeywordValue &&
                    parsed.value === 'block' && parsedAll.length === 1 &&
                    parsedAll[0] instanceof CSSKeywordValue &&
                    getComputedStyle(target).getPropertyValue('display') === 'inline'
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn dataset_is_a_live_dom_string_map_with_web_idl_named_properties() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<main id=target data-foo=one data--bar=dash data-to-string=shadow></main>",
            128,
        )
        .unwrap();
        let value = script(
            &mut engine,
            r#"
                const target = document.getElementById('target');
                const data = target.dataset;
                const initial = data instanceof DOMStringMap && data === target.dataset &&
                    data.foo === 'one' && data.Bar === 'dash' && data.toString === 'shadow' &&
                    'foo' in data && Object.keys(data).sort().join(',') === 'Bar,foo,toString' &&
                    Object.getOwnPropertyDescriptor(data, 'foo').value === 'one';
                target.setAttribute('data-live-value', 'from-attribute');
                const attributeToObject = data.liveValue === 'from-attribute';
                data.coercedValue = {toString() { return 'from-object'; }};
                const objectToAttribute = target.getAttribute('data-coerced-value') === 'from-object';
                const thrown = {};
                let conversionPreservedThrownValue = false;
                try {
                    data.badValue = {toString() { throw thrown; }};
                } catch (error) {
                    conversionPreservedThrownValue = error === thrown;
                }
                let invalidNameWasRejected = false;
                try {
                    data['bad-name'] = 'invalid';
                } catch (error) {
                    invalidNameWasRejected = error.name === 'SyntaxError';
                }
                const beforeDelete = Object.keys(data).includes('Bar') &&
                    Object.getOwnPropertyDescriptor(data, 'liveValue').enumerable;
                delete data.Bar;
                const deletionUpdatesAttributes = !target.hasAttribute('data--bar') &&
                    !('Bar' in data) && !Object.keys(data).includes('Bar');
                data.liveValue = 'from-dataset';
                target.removeAttribute('data-live-value');
                const externalDeletionIsLive = data.liveValue === undefined && !('liveValue' in data);
                const symbol = Symbol('expando');
                data[symbol] = 'symbol-value';
                const adoptedDocument = new DOMParser().parseFromString('<body></body>', 'text/html');
                const adopted = adoptedDocument.adoptNode(target);
                data.afterAdoption = 'still-live';
                const adoptionKeepsDatasetLive = adopted.dataset === data &&
                    adopted.getAttribute('data-after-adoption') === 'still-live';
                initial && attributeToObject && objectToAttribute && conversionPreservedThrownValue &&
                    invalidNameWasRejected && beforeDelete && deletionUpdatesAttributes &&
                    externalDeletionIsLive && data[symbol] === 'symbol-value' && adoptionKeepsDatasetLive
            "#,
        );
        if !matches!(value, Value::Bool(true)) {
            panic!("dataset behavior did not return true");
        }
    }

    #[test]
    fn dataset_keeps_a_detached_element_alive_after_its_wrapper_is_collected() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        script(
            &mut engine,
            r#"
                var detachedData;
                (() => {
                    const element = document.createElement('div');
                    element.setAttribute('data-preserved', 'yes');
                    document.body.appendChild(element);
                    detachedData = element.dataset;
                    document.body.removeChild(element);
                })();
            "#,
        );
        engine.collect_garbage();
        realm.with_session(|_| {});
        let value = script(
            &mut engine,
            "detachedData.preserved === 'yes' && Object.keys(detachedData).join(',') === 'preserved'",
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn current_script_scope_restores_nested_and_detached_script_identity() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<script id=outer data-role=outer></script><script id=inner data-role=inner></script>",
            128,
        )
        .unwrap();
        let (outer, inner) = realm.with_session(|session| {
            let document = session.document();
            (
                selector::query_selector(document, document.root(), "#outer")
                    .unwrap()
                    .unwrap(),
                selector::query_selector(document, document.root(), "#inner")
                    .unwrap()
                    .unwrap(),
            )
        });
        assert!(matches!(
            script(&mut engine, "document.currentScript === null"),
            Value::Bool(true)
        ));
        {
            let outer_scope = realm.enter_script(Some(outer));
            assert_eq!(realm.retained_nodes.borrow().get(&outer), Some(&1));
            // Simulate parser/script removal before any JS wrapper has been requested.
            realm.with_session(|session| session.document_mut().remove(outer).unwrap());
            realm.reap_detached([outer]);
            assert!(realm.session.borrow().document().kind(outer).is_ok());
            assert!(!realm.has_live_wrapper(outer));
            assert!(matches!(
                script(
                    &mut engine,
                    "globalThis.savedOuterScript = document.currentScript; savedOuterScript instanceof HTMLScriptElement && savedOuterScript.dataset.role === 'outer' && document.currentScript === savedOuterScript"
                ),
                Value::Bool(true)
            ));
            {
                let inner_scope = realm.enter_script(Some(inner));
                assert_eq!(realm.retained_nodes.borrow().get(&outer), Some(&1));
                assert_eq!(realm.retained_nodes.borrow().get(&inner), Some(&1));
                assert!(matches!(
                    script(
                        &mut engine,
                        "document.currentScript.id === 'inner' && document.currentScript.dataset.role === 'inner'"
                    ),
                    Value::Bool(true)
                ));
                drop(inner_scope);
            }
            assert!(!realm.retained_nodes.borrow().contains_key(&inner));
            assert!(matches!(
                script(&mut engine, "document.currentScript === savedOuterScript"),
                Value::Bool(true)
            ));
            {
                let module_scope = realm.enter_script(None);
                assert!(matches!(
                    script(&mut engine, "document.currentScript === null"),
                    Value::Bool(true)
                ));
                drop(module_scope);
            }
            assert!(matches!(
                script(&mut engine, "document.currentScript === savedOuterScript"),
                Value::Bool(true)
            ));
            let completion = engine
                .eval_value("throw new Error('script failure')")
                .unwrap();
            assert!(completion.is_err());
            assert!(matches!(
                script(&mut engine, "document.currentScript === savedOuterScript"),
                Value::Bool(true)
            ));
            drop(outer_scope);
        }
        assert!(!realm.retained_nodes.borrow().contains_key(&outer));
        assert!(matches!(
            script(&mut engine, "document.currentScript === null"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn current_script_retention_moves_with_script_adoption() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body><script id=active></script></body>", 64).unwrap();
        let script_node = realm.with_session(|session| {
            selector::query_selector(session.document(), session.document().root(), "#active")
                .unwrap()
                .unwrap()
        });
        let scope = realm.enter_script(Some(script_node));
        let value = script(
            &mut engine,
            r#"
                globalThis.targetDocument = new DOMParser().parseFromString('<body></body>', 'text/html');
                globalThis.adoptedCurrentScript = targetDocument.adoptNode(document.getElementById('active'));
                document.currentScript === adoptedCurrentScript &&
                  adoptedCurrentScript.ownerDocument === targetDocument
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));

        let global = engine.ctx().global_this();
        let target_document = engine
            .ctx()
            .member_get(&global, "targetDocument")
            .unwrap_or_else(|_| panic!("target document is unavailable"));
        let target_realm = engine
            .ctx()
            .with_instance::<DomDocument, _>(&target_document, |document| document.realm.clone())
            .unwrap();
        let adopted_script = engine
            .ctx()
            .member_get(&global, "adoptedCurrentScript")
            .unwrap_or_else(|_| panic!("adopted current script is unavailable"));
        let adopted_node = engine
            .ctx()
            .with_instance::<DomNode, _>(&adopted_script, |node| node.id)
            .unwrap();
        assert!(!realm.retained_nodes.borrow().contains_key(&script_node));
        assert_eq!(
            target_realm.retained_nodes.borrow().get(&adopted_node),
            Some(&1)
        );
        assert_eq!(
            target_realm
                .script_retentions
                .borrow()
                .get(&adopted_node)
                .map(Vec::len),
            Some(1)
        );

        drop(scope);
        assert!(!target_realm
            .retained_nodes
            .borrow()
            .contains_key(&adopted_node));
        assert!(!target_realm
            .script_retentions
            .borrow()
            .contains_key(&adopted_node));
        assert!(matches!(
            script(&mut engine, "document.currentScript === null"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn document_implementation_character_data_and_sibling_operations_use_native_nodes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<body><div></div></body>", 256).unwrap();
        let value = script(
            &mut engine,
            r#"
                const implementation = document.implementation;
                const htmlDoc = implementation.createHTMLDocument(null);
                const titleElement = htmlDoc.documentElement.firstChild.firstChild;
                const title = titleElement.firstChild;
                const doctype = implementation.createDocumentType('svg:svg', 'public-id', 'system-id');
                const xmlDoc = implementation.createDocument('http://www.w3.org/2000/svg', 'svg:svg', doctype);
                const xmlElement = xmlDoc.documentElement;
                const instruction = xmlDoc.createProcessingInstruction('go', 'now');
                const unnamespaced = xmlDoc.createElement('DIV');
                const character = htmlDoc.createTextNode('A💩B');
                character.insertData(1, 'x');
                const selected = character.substringData(2, 2) === '💩';
                character.deleteData(2, 2);
                character.replaceData(2, 1, '!');
                character.appendData(null);
                const body = htmlDoc.body;
                const child = htmlDoc.createElement('i');
                body.append('left', child, 'right');
                const appendPreservesNode = body.childNodes.length === 3 &&
                    body.firstChild.textContent === 'left' && body.firstChild.nextSibling === child &&
                    body.lastChild.textContent === 'right';
                child.before('before');
                const beforeInsertsText = child.previousSibling && child.previousSibling.textContent === 'before';
                child.after('after');
                const afterInsertsText = child.nextSibling && child.nextSibling.textContent === 'after';
                child.replaceWith('replaced');
                const replaceWithReplacesNode = child.parentNode === null &&
                    body.innerHTML === 'leftbeforereplacedafterright';
                body.prepend('first');
                const host = document.querySelector('div');
                const appRoot = document.createElement('div');
                const appMessage = document.createElement('div');
                appMessage.id = 'native-app-message';
                appRoot.appendChild(appMessage);
                host.replaceChildren(appRoot);
                const replaceChildrenPreservesHtmlNode =
                    document.getElementById('native-app-message') === appMessage;
                let windowReceiver;
                window.addEventListener.call(null, 'null-this', function () { windowReceiver = this; }, {once:true});
                window.dispatchEvent(new Event('null-this', {bubbles:true}));
                let detachedWindowReceiver = false;
                const detachedWindowListener = () => { detachedWindowReceiver = true; };
                window.addEventListener('detached-document-event', detachedWindowListener, {once:true});
                body.dispatchEvent(new Event('detached-document-event', {bubbles:true}));
                window.removeEventListener('detached-document-event', detachedWindowListener);
                const failures = [];
                const check = (name, condition) => { if (!condition) failures.push(name); };
                check('cached document.implementation identity', implementation === document.implementation);
                check('DOMImplementation.hasFeature', implementation.hasFeature() === true);
                check('createHTMLDocument Document type', htmlDoc instanceof Document);
                check('detached createHTMLDocument defaultView', htmlDoc.defaultView === null);
                check('createHTMLDocument contentType', htmlDoc.contentType === 'text/html');
                check('createHTMLDocument DOMString title conversion', title.data === 'null');
                check('HTMLHtmlElement wrapper', htmlDoc.documentElement instanceof HTMLHtmlElement);
                check('HTMLHeadElement wrapper', htmlDoc.documentElement.firstChild instanceof HTMLHeadElement);
                check('HTMLBodyElement wrapper', htmlDoc.body instanceof HTMLBodyElement);
                check('HTMLTitleElement wrapper', titleElement instanceof HTMLTitleElement);
                check('createDocument XMLDocument type', xmlDoc instanceof XMLDocument);
                check('doctype retained in created document', xmlDoc.doctype === doctype);
                check('doctype ownerDocument after adoption', doctype.ownerDocument === xmlDoc);
                check('SVG document contentType', xmlDoc.contentType === 'image/svg+xml');
                check('createElementNS prefix', xmlElement.prefix === 'svg');
                check('createElementNS localName', xmlElement.localName === 'svg');
                check('XML createElement preserves case', unnamespaced.localName === 'DIV');
                check('XML createElement null namespace', unnamespaced.namespaceURI === null);
                check('ProcessingInstruction wrapper', instruction instanceof ProcessingInstruction);
                check('ProcessingInstruction CharacterData inheritance', instruction instanceof CharacterData);
                check('ProcessingInstruction target', instruction.target === 'go');
                check('ProcessingInstruction data', instruction.data === 'now');
                check('CharacterData UTF-16 substringData', selected);
                check('CharacterData UTF-16 mutation and DOMString conversion',
                    character instanceof CharacterData && character.length === 7 && character.data === 'Ax!null');
                check('Node.hasChildNodes', body.hasChildNodes());
                check('ParentNode append retains derived HTML node identity', appendPreservesNode);
                check('ChildNode before inserts a text node', beforeInsertsText);
                check('ChildNode after inserts a text node', afterInsertsText);
                check('ChildNode replaceWith replaces with text', replaceWithReplacesNode);
                check('ParentNode prepend inserts text at the beginning', body.firstChild.textContent === 'first');
                check('ParentNode replaceChildren retains connected HTML node identity', replaceChildrenPreservesHtmlNode);
                check('ParentNode mixed-string serialization',
                    body.innerHTML === 'firstleftbeforereplacedafterright');
                check('Window null receiver resolves to global Window', windowReceiver === window);
                check('detached document event does not bubble to active Window', !detachedWindowReceiver);
                if (failures.length) throw new Error('DOM batch regression failed: ' + failures.join(', '));
                true
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn processing_instruction_xml_target_is_allowed_for_lenient_serialization() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let value = script(
            &mut engine,
            r#"
                const xml = new DOMParser().parseFromString('<root/>', 'text/xml');
                const instruction = xml.createProcessingInstruction('xml', 'b');
                const serialized = new XMLSerializer().serializeToString(instruction);
                instruction instanceof ProcessingInstruction &&
                    instruction.target === 'xml' && serialized === '<?xml b?>'
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn document_constructor_and_cdata_sections_create_real_xml_nodes() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let active_realm = install(engine.ctx(), "<main></main>", 64).unwrap();
        active_realm.set_document_url("https://document-origin.example/path/page.html");
        let result = script(
            engine,
            r#"
                (() => {
                    const xml = new Document();
                    const root = xml.createElementNS(null, 'root');
                    const cdata = xml.createCDATASection('<raw>&text');
                    xml.appendChild(root);
                    root.appendChild(cdata);
                    cdata.appendData('!');
                    const range = xml.createRange();
                    range.selectNodeContents(cdata);
                    const rangeCopy = range.cloneContents().firstChild;
                    let htmlError;
                    try { document.createCDATASection('forbidden'); }
                    catch (error) { htmlError = error; }
                    let dataError;
                    try { xml.createCDATASection('bad]]>data'); }
                    catch (error) { dataError = error; }
                    return xml instanceof Document && !(xml instanceof XMLDocument) &&
                        xml.contentType === 'application/xml' && xml.URL === 'about:blank' &&
                        xml.defaultView === null && xml.documentElement === root &&
                        xml.firstChild === root && root.firstChild === cdata &&
                        root.childNodes[0] === cdata && cdata instanceof CDATASection &&
                        cdata instanceof Text && cdata instanceof CharacterData &&
                        cdata.nodeType === 4 && cdata.nodeName === '#cdata-section' &&
                        cdata.data === '<raw>&text!' && cdata.textContent === '<raw>&text!' &&
                        root.textContent === '<raw>&text!' &&
                        root.innerHTML === '<![CDATA[<raw>&text!]]>' &&
                        range.toString() === '<raw>&text!' &&
                        rangeCopy instanceof CDATASection && rangeCopy.data === cdata.data &&
                        xml.createCDATASection('again') instanceof CDATASection &&
                        htmlError && htmlError.name === 'NotSupportedError' &&
                        dataError && dataError.name === 'InvalidCharacterError';
                })()
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));

        let detached_document = script(engine, "new Document()");
        let captured_origin = engine
            .ctx()
            .with_instance::<DomDocument, _>(&detached_document, |document| {
                document.realm.document_origin()
            })
            .unwrap();
        let active_origin = active_realm.document_origin();
        assert_eq!(captured_origin, active_origin);
    }

    #[test]
    fn xml_parsing_and_document_factories_keep_their_declared_interfaces() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
                (() => {
                    const parser = new DOMParser();
                    const mimeTypes = [
                        'text/xml', 'application/xml',
                        'application/xhtml+xml', 'image/svg+xml'
                    ];
                    const parsed = mimeTypes.map(type =>
                        parser.parseFromString('<root/>', type));
                    const failed = mimeTypes.map(type =>
                        parser.parseFromString('<root>', type));
                    const parserDocuments = [...parsed, ...failed].every(doc =>
                        doc instanceof Document && !(doc instanceof XMLDocument) &&
                        doc.readyState === 'complete');
                    const htmlParsed = parser.parseFromString('<p/>', 'text/html');

                    const ordinary = new Document();
                    const ordinaryClone = ordinary.cloneNode(false);
                    const implementationXml = document.implementation.createDocument(
                        null, 'root');
                    const xmlClone = implementationXml.cloneNode(false);
                    return parserDocuments &&
                        htmlParsed instanceof Document &&
                        !(htmlParsed instanceof XMLDocument) &&
                        htmlParsed.readyState === 'complete' &&
                        document.readyState === 'loading' &&
                        ordinary instanceof Document && !(ordinary instanceof XMLDocument) &&
                        ordinary.readyState === 'complete' &&
                        ordinaryClone instanceof Document && !(ordinaryClone instanceof XMLDocument) &&
                        ordinaryClone.readyState === 'complete' &&
                        implementationXml instanceof XMLDocument &&
                        implementationXml.readyState === 'complete' &&
                        xmlClone instanceof XMLDocument &&
                        parsed[2].contentType === 'application/xhtml+xml' &&
                        parsed[3].contentType === 'image/svg+xml';
                })()
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn dynamic_module_activations_capture_fifo_source_base_and_detached_targets() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<base href='/early/'>", 128).unwrap();
        realm.set_document_url("https://modules.test/page.html");
        realm.enable_module_script_activations();
        script(
            &mut engine,
            r#"
            globalThis.moduleLoads = [];
            globalThis.firstModule = document.createElement('script');
            firstModule.type = 'module';
            firstModule.async = false;
            firstModule.text = 'export const prepared = 1;';
            firstModule.onload = event => moduleLoads.push(event.target === firstModule && event.isTrusted);
            document.body.appendChild(firstModule);
            firstModule.text = 'export const changed = 2;';
            firstModule.remove();
            document.querySelector('base').setAttribute('href', '/later/');
            globalThis.secondModule = document.createElement('script');
            secondModule.type = 'module';
            secondModule.src = 'entry.js';
            document.body.appendChild(secondModule);
            secondModule.src = 'changed.js';
            secondModule.remove();
        "#,
        );
        let activations = realm.drain_script_activations(engine.ctx());
        assert_eq!(activations.len(), 2);
        assert_eq!(activations[0].script.text, "export const prepared = 1;");
        assert_eq!(activations[0].base_url, "https://modules.test/early/");
        assert!(!activations[0].script.is_async);
        assert!(!activations[0].script.parser_inserted);
        assert_eq!(activations[0].script.namespace, Namespace::Html);
        assert_eq!(activations[1].script.src.as_deref(), Some("entry.js"));
        assert_eq!(activations[1].base_url, "https://modules.test/later/");
        assert!(activations[1].script.is_async);
        assert!(realm.drain_script_activations(engine.ctx()).is_empty());
        assert!(realm.unhandled_script_activations().is_empty());
        realm.with_session(|_| ());
        script(
            &mut engine,
            "document.implementation.createHTMLDocument('adopted').adoptNode(firstModule);",
        );
        let (owner, node) = activations[0].current_node();
        assert!(!Rc::ptr_eq(&owner, &realm));
        assert_eq!(owner.retained_nodes.borrow().get(&node), Some(&1));
        assert!(activations[0]
            .dispatch_terminal(engine.ctx(), "load")
            .unwrap());
        assert!(matches!(
            script(&mut engine, "moduleLoads.length === 1 && moduleLoads[0]"),
            Value::Bool(true)
        ));
        drop(activations);
        assert!(!owner.retained_nodes.borrow().contains_key(&node));
    }

    #[test]
    fn html_base_elements_reflect_attributes_and_freeze_the_effective_base() {
        // Form submission uses the runtime's canonical FormData implementation;
        // this regression exercises action URL resolution through that path.
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let realm = install(
            engine.ctx(),
            r#"<html><head>
                <base id="first" href="/one/">
                <base id="second" href="/two/">
                <base id="empty">
              </head><body>
                <form id="form" action="submit"></form>
              </body></html>"#,
            128,
        )
        .unwrap();
        realm.set_document_url("https://example.test/dir/page.html");

        let submitted_action = Rc::new(RefCell::new(None::<String>));
        let capture_action = submitted_action.clone();
        realm.set_form_submission_host(Rc::new(move |request| {
            *capture_action.borrow_mut() = Some(request.metadata.action);
            Ok(())
        }));

        let value = script(
            engine,
            r#"
            (() => {
                const first = document.getElementById('first');
                const second = document.getElementById('second');
                const empty = document.getElementById('empty');
                const form = document.getElementById('form');
                const checks = [];
                const check = (name, ok) => { if (!ok) checks.push(name); };
                check('FormData global identity', typeof FormData === 'function' && globalThis.FormData === FormData);
                check('HTMLBaseElement wrapper', first instanceof HTMLBaseElement);
                check('base href uses fallback base', first.href === 'https://example.test/one/');
                check('second href uses fallback base', second.href === 'https://example.test/two/');
                check('missing href resolves against fallback base', empty.href === 'https://example.test/dir/page.html');
                first.target = '_blank';
                check('target reflects through the content attribute', first.getAttribute('target') === '_blank');
                first.target = 17;
                check('target setter converts to DOMString', first.target === '17');
                check('first base controls document.baseURI', document.baseURI === 'https://example.test/one/');
                form.requestSubmit();

                first.parentNode.insertBefore(second, first);
                check('tree reordering selects the new first base', document.baseURI === 'https://example.test/two/');
                second.href = 'updated/';
                check('active href mutation refreezes against fallback', document.baseURI === 'https://example.test/dir/updated/');
                first.href = '/changed-first/';
                check('inactive href mutation leaves active base unchanged', document.baseURI === 'https://example.test/dir/updated/');
                if (checks.length) throw new Error(checks.join(', '));
                return true;
            })()
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        assert_eq!(
            submitted_action.borrow().as_deref(),
            Some("https://example.test/one/submit")
        );

        realm.set_document_url("https://other.test/new/page.html");
        let value = script(
            engine,
            r#"
            (() => {
                const checks = [];
                const check = (name, ok) => { if (!ok) checks.push(name); };
                const first = document.getElementById('first');
                const second = document.getElementById('second');
                check('active frozen base survives document URL update', document.baseURI === 'https://example.test/dir/updated/');
                first.href = '/changed-again/';
                check('secondary href mutation preserves the frozen active base', document.baseURI === 'https://example.test/dir/updated/');
                const late = document.createElement('base');
                late.href = '/late/';
                first.parentNode.appendChild(late);
                check('later base insertion preserves the selected base', document.baseURI === 'https://example.test/dir/updated/');
                late.remove();
                first.parentNode.appendChild(first);
                check('reordering non-active bases preserves the selected base', document.baseURI === 'https://example.test/dir/updated/');
                second.href = 'refrozen/';
                check('active href mutation refreezes against current fallback', document.baseURI === 'https://other.test/new/refrozen/');
                second.remove();
                check('promoted base freezes against current fallback', document.baseURI === 'https://other.test/changed-again/');
                first.removeAttribute('href');
                check('removing final href restores document fallback', document.baseURI === 'https://other.test/new/page.html');
                if (checks.length) throw new Error(checks.join(', '));
                return true;
            })()
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn html_base_ignores_forbidden_urls_and_non_html_namespaces() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            r#"<html><head>
                <base id="candidate" href="data:text/plain,blocked">
                <svg xmlns="http://www.w3.org/2000/svg"><base id="svg-base" href="/svg/"></base></svg>
                <base id="eligible" href="/eligible/">
              </head><body></body></html>"#,
            128,
        )
        .unwrap();
        realm.set_document_url("https://example.test/dir/page.html");

        let value = script(
            &mut engine,
            r#"
                const candidate = document.getElementById('candidate');
                const svgBase = document.getElementById('svg-base');
                const eligible = document.getElementById('eligible');
                const fallback = 'https://example.test/dir/page.html';
                const checks = [];
                const check = (name, ok) => { if (!ok) checks.push(name); };
                check('data URL is not an effective base', document.baseURI === fallback);
                check('foreign namespace element is not HTMLBaseElement', !(svgBase instanceof HTMLBaseElement));
                check('foreign namespace base does not win', document.baseURI !== 'https://example.test/svg/');
                candidate.setAttribute('href', 'javascript:alert(1)');
                check('javascript URL is not an effective base', document.baseURI === fallback);
                candidate.setAttribute('href', 'http://[');
                check('unparseable URL falls back', document.baseURI === fallback);
                check('invalid href getter preserves raw content', candidate.href === 'http://[');
                candidate.setAttributeNS('urn:custom', 'href', '/custom/');
                check('custom namespace href preserves the null-namespace attribute', document.baseURI === fallback);
                candidate.removeAttribute('href');
                check('custom namespace href does not select a base', document.baseURI === 'https://example.test/eligible/');
                candidate.setAttributeNS(null, 'HREF', '/upper/');
                check('HTML mode folds href attribute case', document.baseURI === 'https://example.test/upper/');
                check('base href getter folds HTML attribute case', candidate.href === 'https://example.test/upper/');
                candidate.remove();
                check('next HTML base becomes active', document.baseURI === 'https://example.test/eligible/');
                check('eligible HTML element has native interface', eligible instanceof HTMLBaseElement);
                if (checks.length) throw new Error(checks.join(', '));
                true
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn xhtml_and_inherited_about_documents_use_their_base_context() {
        let mut engine = Engine::new();
        let realm = install_xhtml(
            engine.ctx(),
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><base HREF="/ignored/"></base><base id="xhtml-base" href="/xhtml/"></base></head><body/></html>"#,
            64,
        )
        .unwrap();
        realm.set_document_url("https://xml.test/page.xhtml");
        let value = script(
            &mut engine,
            r#"document.getElementById('xhtml-base') instanceof HTMLBaseElement && document.baseURI === 'https://xml.test/xhtml/'"#,
        );
        assert!(matches!(value, Value::Bool(true)));

        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<base id='child-base' href='../child/'>", 64).unwrap();
        realm.set_document_url("about:srcdoc");
        realm.set_about_base_url(Some("https://parent.test/dir/page.html".into()));
        let value = script(
            &mut engine,
            r#"
                const base = document.getElementById('child-base');
                document.URL === 'about:srcdoc'
                    && base.href === 'https://parent.test/child/'
                    && document.baseURI === 'https://parent.test/child/'
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));

        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.set_document_url("about:blank");
        realm.set_about_base_url(Some("https://creator.test/path/document.html".into()));
        assert!(matches!(
            script(
                &mut engine,
                "document.URL === 'about:blank' && document.baseURI === 'https://creator.test/path/document.html'"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn invalid_script_source_queues_trusted_error_at_preparation_without_fetch() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<body></body>", 64).unwrap();
        realm.enable_resource_script_activations();
        let synchronous = script(&mut engine, r#"
            globalThis.preparationEvents = [];
            for (const src of ['', ' ', '\t', '\n', '\f', '\r', ' \t\n\f\r ']) {
                const element = document.createElement('script');
                element.src = src;
                element.onerror = event => preparationEvents.push(
                    event.isTrusted && event.target === element && !event.bubbles && !event.cancelable);
                element.onload = () => preparationEvents.push('load');
                document.body.append(element);
                element.remove();
            }
            preparationEvents.length === 0;
        "#);
        assert!(matches!(synchronous, Value::Bool(true)));
        assert!(realm.drain_script_activations(engine.ctx()).is_empty());
        assert_eq!(realm.retained_nodes.borrow().len(), 7);
        engine.collect_garbage();
        assert!(scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(script(&mut engine,
            "preparationEvents.length === 7 && preparationEvents.every(value => value === true)"),
            Value::Bool(true)));
        assert!(realm.retained_nodes.borrow().is_empty());
    }

    #[test]
    fn parser_script_target_lease_survives_removal_until_terminal_event() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<script id='pending' type='module' src='entry.js'></script>",
            64,
        )
        .unwrap();
        realm.set_document_url("https://modules.test/parser.html");
        let node = realm.document_scripts()[0].node;
        let lease = realm.retain_script_target(engine.ctx(), node).unwrap();
        assert!(lease.script.parser_inserted);
        assert!(!lease.script.is_async);
        script(
            &mut engine,
            "globalThis.pendingScript = document.getElementById('pending'); pendingScript.onerror = event => { globalThis.removedScriptError = event.target === pendingScript && event.isTrusted; }; pendingScript.remove();",
        );
        realm.with_session(|_| ());
        assert_eq!(realm.retained_nodes.borrow().get(&node), Some(&1));
        assert!(lease.dispatch_terminal(engine.ctx(), "error").unwrap());
        assert!(matches!(
            script(&mut engine, "removedScriptError"),
            Value::Bool(true)
        ));
        drop(lease);
        assert!(!realm.retained_nodes.borrow().contains_key(&node));
    }

    #[test]
    fn native_webidl_interfaces_preserve_global_and_prototype_descriptors() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(
            script(
                &mut engine,
                r#"
            const descriptorOf = Object.getOwnPropertyDescriptor;
            globalThis.savedDescriptorOf = descriptorOf;
            let valid = true;
            for (const name of ['Event', 'EventTarget', 'Node', 'Document', 'Element',
                                'FontFace', 'FontFaceSet', 'DOMRect', 'MutationObserver', 'Path2D']) {
                const descriptor = descriptorOf(globalThis, name);
                valid = valid && !!descriptor && !descriptor.enumerable && descriptor.writable && descriptor.configurable;
            }
            for (const [prototype, name] of [[Event.prototype, 'type'], [Node.prototype, 'nodeType'],
                                            [FontFace.prototype, 'family'], [EventTarget.prototype, 'addEventListener']]) {
                const descriptor = descriptorOf(prototype, name);
                valid = valid && !!descriptor && descriptor.enumerable && descriptor.configurable;
            }
            const iterator = descriptorOf(FontFaceSet.prototype, Symbol.iterator);
            valid = valid && !!iterator && !iterator.enumerable && iterator.writable && iterator.configurable;
            const constant = descriptorOf(Node.prototype, 'ELEMENT_NODE');
            valid = valid && constant.value === 1 && constant.enumerable && !constant.writable && !constant.configurable;
            Object.defineProperty(globalThis, 'nativeInterfaceAlias', {
                configurable: true,
                get() { throw new Error('author getter'); },
                set() { throw new Error('author setter'); }
            });
            for (const key of ['value', 'writable', 'enumerable', 'configurable']) {
                const descriptor = Object.create(null);
                descriptor.configurable = true;
                descriptor.set = () => { throw new Error('author descriptor setter'); };
                Object.defineProperty(Object.prototype, key, descriptor);
            }
            Object.defineProperty = () => { throw new Error('author defineProperty'); };
            valid
        "#
            ),
            Value::Bool(true)
        ));
        let global = engine.ctx().global_object();
        let constructor = engine
            .ctx()
            .member_get(&global, "Event")
            .ok()
            .expect("native Event constructor");
        install_interface(engine.ctx(), &global, "nativeInterfaceAlias", constructor)
            .ok()
            .expect("intrinsic interface installation");
        assert!(matches!(
            script(
                &mut engine,
                r#"
            const alias = savedDescriptorOf(globalThis, 'nativeInterfaceAlias');
            alias.value === Event && !alias.enumerable && alias.writable && alias.configurable
        "#
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn native_exception_reporting_preserves_payload_and_captured_constructor() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        script(
            &mut engine,
            r#"
            const SavedErrorEvent = ErrorEvent;
            globalThis.actualException = new Error('original failure');
            globalThis.reportedErrors = [];
            window.addEventListener('error', event => {
                reportedErrors.push(event);
                event.preventDefault();
            });
            globalThis.ErrorEvent = function() { throw new Error('page constructor must not run'); };
            const nativeReport = reportError;
            nativeReport(actualException);
            globalThis.reportError = function() { throw new Error('page reporter must not run'); };
            globalThis.reportingContract = reportedErrors.length === 1 &&
                reportedErrors[0] instanceof SavedErrorEvent && reportedErrors[0] instanceof Event &&
                reportedErrors[0].isTrusted && reportedErrors[0].cancelable &&
                reportedErrors[0].error === actualException && reportedErrors[0].message === 'original failure';
        "#,
        );
        assert!(matches!(
            script(&mut engine, "reportingContract"),
            Value::Bool(true)
        ));
        let global = engine.ctx().global_object();
        let exception = engine
            .ctx()
            .member_get(&global, "actualException")
            .ok()
            .expect("original exception");
        DomRealm::report_exception(engine.ctx(), exception);
        assert!(matches!(
            script(
                &mut engine,
                "reportedErrors.length === 2 && reportedErrors[1].error === actualException"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn active_xml_install_preserves_namespaces_and_native_svg_script_descriptors() {
        let mut engine = Engine::new();
        let realm = install_xml(
            engine.ctx(),
            r#"<s:svg xmlns:s="http://www.w3.org/2000/svg"
                     xmlns:h="http://www.w3.org/1999/xhtml"
                     xmlns:l="http://www.w3.org/1999/xlink">
                <h:script src="harness.js"/>
                <s:script href="preferred.js" l:href="ignored.js"/>
                <s:script l:href="legacy.js"/>
                <s:script><![CDATA[globalThis.fromCdata = '<node>&';]]></s:script>
                <s:script type="module">export const value = 1;</s:script>
                <s:SCRIPT>globalThis.wrongCase = true;</s:SCRIPT>
            </s:svg>"#,
            128,
            XmlDocumentType::Svg,
        )
        .unwrap();
        let descriptors = realm.document_scripts();
        assert_eq!(descriptors.len(), 5);
        assert_eq!(descriptors[0].src.as_deref(), Some("harness.js"));
        assert_eq!(descriptors[1].src.as_deref(), Some("preferred.js"));
        assert_eq!(descriptors[2].src.as_deref(), Some("legacy.js"));
        assert_eq!(descriptors[3].text, "globalThis.fromCdata = '<node>&';");
        assert_eq!(descriptors[4].kind, ScriptType::Module);
        assert!(descriptors
            .iter()
            .all(|script| script.parser_inserted && !script.is_async));
        let value = script(
            &mut engine,
            r#"
            document instanceof Document && !(document instanceof XMLDocument) &&
            document.defaultView === window && window instanceof Window &&
            document.contentType === 'image/svg+xml' && document.body === null &&
            document.documentElement.namespaceURI === 'http://www.w3.org/2000/svg' &&
            document.documentElement.localName === 'svg' &&
            document.createElement('plain').namespaceURI === null &&
            document.documentElement.firstElementChild instanceof HTMLScriptElement
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        let value = script(
            &mut engine,
            r#"
            globalThis.svgRuns = 0;
            const dynamic = document.createElementNS('http://www.w3.org/2000/svg', 's:script');
            dynamic.textContent = 'globalThis.svgRuns++; globalThis.svgCurrentScript = document.currentScript;';
            document.documentElement.appendChild(dynamic);
            svgRuns === 1 && svgCurrentScript === dynamic && document.currentScript === null
        "#,
        );
        assert!(matches!(value, Value::Bool(true)));
        assert!(matches!(
            install_xml(engine.ctx(), "<s:svg>", 64, XmlDocumentType::Svg),
            Err(InstallError::XmlParse(_))
        ));
    }

    #[test]
    fn user_agent_events_preserve_native_trust_rejection_payload_and_cancellation() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<script id='target'></script>", 64).unwrap();
        script(
            &mut engine,
            r#"
            const coerced = new Event(17, { bubbles: 1, cancelable: 'yes', composed: {} });
            if (coerced.type !== '17' || !coerced.bubbles || !coerced.cancelable || !coerced.composed)
                throw new Error('Event IDL coercion');
            const sentinel = {};
            let caught;
            try { new Event('x', { get bubbles() { throw sentinel; } }); }
            catch (error) { caught = error; }
            if (caught !== sentinel) throw new Error('Event dictionary getter exception identity');
            let windowLoadCount = 0;
            window.onload = event => {
                if (event.currentTarget !== window) throw new Error('Window load receiver');
                windowLoadCount++;
            };
            window.dispatchEvent(new Event('load'));
            window.onload = null;
            window.dispatchEvent(new Event('load'));
            if (windowLoadCount !== 1) throw new Error('Window load handler lifecycle');
            globalThis.eventTrust = [];
            const target = document.getElementById('target');
            target.onload = event => { eventTrust.push(event.isTrusted); globalThis.lastUaEvent = event; };
            globalThis.reason = { token: 7 };
            globalThis.rejected = Promise.reject(reason);
            window.onunhandledrejection = event => {
                globalThis.rejectionEvent = event;
                event.preventDefault();
            };
            window.onrejectionhandled = event => { globalThis.handledEvent = event; };
        "#,
        );
        let node = realm.document_scripts()[0].node;
        script(
            &mut engine,
            "window.onload = event => { globalThis.nativeWindowLoadTrusted = event.isTrusted && event.target === document && event.currentTarget === window && event.composedPath().length === 1 && event.composedPath()[0] === window; };",
        );
        assert!(realm
            .dispatch_window_user_agent(engine.ctx(), "load", false, false)
            .unwrap());
        assert!(matches!(
            script(&mut engine, "nativeWindowLoadTrusted === true"),
            Value::Bool(true)
        ));
        assert!(realm
            .dispatch_user_agent(engine.ctx(), node, "load", false, false, &[])
            .unwrap());
        assert!(matches!(
            script(
                &mut engine,
                "eventTrust.join(',') === 'true' && lastUaEvent.isTrusted && !new Event('load').isTrusted"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            script(
                &mut engine,
                "document.getElementById('target').dispatchEvent(lastUaEvent); eventTrust.join(',') === 'true,false' && !lastUaEvent.isTrusted"
            ),
            Value::Bool(true)
        ));
        let global = engine.ctx().global_object();
        let promise = engine
            .ctx()
            .member_get(&global, "rejected")
            .ok()
            .expect("rejected promise");
        let reason = engine
            .ctx()
            .member_get(&global, "reason")
            .ok()
            .expect("rejection reason");
        assert!(!realm
            .dispatch_promise_rejection(
                engine.ctx(),
                "unhandledrejection",
                promise.clone(),
                reason.clone()
            )
            .unwrap());
        assert!(matches!(
            script(
                &mut engine,
                r#"
            rejectionEvent instanceof PromiseRejectionEvent && rejectionEvent instanceof Event &&
            rejectionEvent.target === window && rejectionEvent.isTrusted && rejectionEvent.cancelable &&
            rejectionEvent.defaultPrevented && rejectionEvent.promise === rejected && rejectionEvent.reason === reason
        "#
            ),
            Value::Bool(true)
        ));
        assert!(realm
            .dispatch_promise_rejection(engine.ctx(), "rejectionhandled", promise, reason)
            .unwrap());
        assert!(matches!(
            script(
                &mut engine,
                "handledEvent.isTrusted && !handledEvent.cancelable && handledEvent.promise === rejected && handledEvent.reason === reason"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn xhtml_install_uses_xml_parsing_and_active_xml_document_bindings() {
        let mut engine = Engine::new();
        install_xhtml(
            engine.ctx(),
            r#"<?xml version="1.0"?>
                <html xmlns="http://www.w3.org/1999/xhtml">
                  <head/>
                  <body>
                    <MiXeD id="mixed"><![CDATA[<node>&]]></MiXeD>
                    <script id="source">const value = "&lt;node&gt;";</script>
                  </body>
                </html>"#,
            128,
        )
        .unwrap();

        let value = script(
            &mut engine,
            r#"
                const failures = [];
                const check = (name, condition) => { if (!condition) failures.push(name); };
                const mixed = document.getElementById('mixed');
                const created = document.createElement('DIV');
                const body = document.body;
                if (body) body.appendChild(created);
                const scriptElement = document.getElementById('source');
                const scriptSource = scriptElement && scriptElement.textContent;
                const root = document.documentElement;
                check('Document interface', document instanceof Document &&
                    !(document instanceof XMLDocument));
                check('XHTML contentType', document.contentType === 'application/xhtml+xml');
                check('active defaultView', document.defaultView === window);
                check('window.document identity', window.document === document);
                check('root element exists', root !== null);
                check('XHTML root namespace', root !== null && root.namespaceURI === 'http://www.w3.org/1999/xhtml');
                check('root localName case', root !== null && root.localName === 'html');
                check('body exists', body !== null);
                check('mixed element exists', mixed !== null);
                check('mixed element namespace', mixed !== null && mixed.namespaceURI === 'http://www.w3.org/1999/xhtml');
                check('HTML namespace wrapper', mixed !== null && mixed instanceof HTMLElement);
                check('mixed localName case', mixed !== null && mixed.localName === 'MiXeD');
                check('mixed tagName case', mixed !== null && mixed.tagName === 'MiXeD');
                check('CDATA text content', mixed !== null && mixed.textContent === '<node>&');
                check('created element HTML namespace', created.namespaceURI === 'http://www.w3.org/1999/xhtml');
                check('created element localName case', created.localName === 'DIV');
                check('created element tagName case', created.tagName === 'DIV');
                check('script element exists', scriptElement !== null);
                check('entity-decoded script text', scriptSource === 'const value = "<node>";');
                globalThis.xhtmlInstallFailures = failures;
                failures.length === 0
            "#,
        );
        if !matches!(value, Value::Bool(true)) {
            let diagnostic = script(&mut engine, "xhtmlInstallFailures.join(', ')");
            let diagnostic = match diagnostic {
                Value::Str(value) => value.as_str().to_owned(),
                _ => "<diagnostic was not a string>".into(),
            };
            panic!("XHTML installation assertions failed: {diagnostic}");
        }

        let mut invalid_engine = Engine::new();
        assert!(matches!(
            install_xhtml(invalid_engine.ctx(), "<html><body></html>", 32),
            Err(InstallError::XmlParse(_))
        ));
    }

    #[test]
    fn element_traversal_skips_non_elements_and_tracks_moves() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<main>lead<!--a--><b>A</b>between<i>B</i><!--z-->tail</main>",
            64,
        )
        .unwrap();
        let result = script(
            &mut engine,
            "const main = document.querySelector('main'); const b = main.firstElementChild; const i = main.lastElementChild; const before = main.childElementCount === 2 && b.tagName === 'B' && i.tagName === 'I' && b.nextElementSibling === i && i.previousElementSibling === b && b.previousElementSibling === null && i.nextElementSibling === null && main.firstChild.nextElementSibling === b && b.parentElement === main && main.firstChild.parentElement === main && document.documentElement.parentElement === null; main.appendChild(b); before && main.firstElementChild === i && main.lastElementChild === b && b.previousElementSibling === i && i.nextElementSibling === b && main.childElementCount === 2",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn standard_handler_properties_normalize_framework_events_and_retain_order() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<input>", 32).unwrap();
        let result = script(
            &mut engine,
            "const input = document.querySelector('input'); const types = ['keydown','keyup','keypress','beforeinput','change','focus','blur','focusin','focusout','dblclick','pointerdown','pointerup','pointermove','pointercancel','wheel','scroll','compositionstart','compositionupdate','compositionend']; const native = types.every(type => ('on'+type) in input && input['on'+type] === null); const order=[]; input.addEventListener('keydown',()=>order.push('first')); input.onkeydown=()=>order.push('handler'); input.addEventListener('keydown',()=>order.push('last')); input.onkeydown=()=>order.push('replacement'); input.dispatchEvent(new Event('keydown')); const before = order.join(',') === 'first,replacement,last'; input.onkeydown=null; input.dispatchEvent(new Event('keydown')); native && before && order.join(',') === 'first,replacement,last,first,last' && input.onkeydown === null",
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn events_capture_bubble_once_passive_and_survive_gc() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><button></button></main>", 64).unwrap();
        engine.eval_value("var order = []; var button = document.querySelector('button'); var main = button.parentNode; main.addEventListener('click', e => order.push('capture:' + e.eventPhase), true); button.addEventListener('click', e => { order.push('target:' + e.eventPhase); e.preventDefault(); }, { once: true }); main.addEventListener('click', e => { order.push('bubble:' + e.eventPhase); e.preventDefault(); }, { passive: true });").unwrap().ok().unwrap();
        engine.collect_garbage();
        let value = engine.eval_value("const e = new Event('click', {bubbles: true, cancelable: true}); const first = button.dispatchEvent(e); const target = e.target === button && e.currentTarget === null && e.eventPhase === 0; const second = button.dispatchEvent(new Event('click', {bubbles: true, cancelable: true})); !first && second && target && order.join(',') === 'capture:1,target:2,bubble:3,capture:1,bubble:3'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn expando_wrappers_remain_identical_across_collection() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        engine
            .eval_value("document.querySelector('div').saved = 42")
            .unwrap()
            .ok()
            .unwrap();
        engine.collect_garbage();
        let value = engine
            .eval_value("document.querySelector('div').saved === 42")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn host_dispatch_returns_an_error_for_destroyed_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div></div>", 64).unwrap();
        let node = realm.with_session(|session| {
            let node =
                selector::query_selector(session.document(), session.document().root(), "div")
                    .unwrap()
                    .unwrap();
            session.document_mut().remove(node).unwrap();
            session.document_mut().destroy_subtree(node).unwrap();
            node
        });
        assert!(realm
            .dispatch(engine.ctx(), node, "click", true, true, &[])
            .is_err());
    }

    #[test]
    fn style_declarations_and_static_queries_share_the_arena() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<main><b style='color:red'>old</b></main>",
            64,
        )
        .unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const list = main.querySelectorAll('b'); const b = list[0]; const style = b.style; style.width = '12px'; style.setProperty('height','8px','important'); const computed = getComputedStyle(b); main.innerHTML = ''; style instanceof CSSStyleDeclaration && style === b.style && style.width === '12px' && style.getPropertyPriority('height') === 'important' && computed.width === '12px' && computed.color === 'rgb(255, 0, 0)' && list.length === 1 && list[0] === b && b.textContent === 'old'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
        assert!(realm.wrapper_count() < 10);
    }

    #[test]
    fn variadic_mutations_validate_before_moving_nodes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><i></i></main>", 64).unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const old = main.firstChild; const a = document.createElement('a'); const b = document.createElement('b'); let types = false; let hierarchy = false; try { main.append(a, {toString(){throw new Error('string coercion failed');}}); } catch(e) { types = true; } try { main.append(b, main); } catch(e) { hierarchy = true; } const unchanged = a.parentNode === null && b.parentNode === null && main.firstChild === old; main.replaceChildren(a, b); types && hierarchy && unchanged && main.firstChild === a && main.lastChild === b && old.parentNode === null && main.childNodes.length === 2").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn events_reach_window_and_static_lists_retain_untouched_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><b>retained</b></main>", 64).unwrap();
        assert_eq!(realm.wrapper_count(), 0);
        let value = engine.eval_value("const main = document.querySelector('main'); const list = document.querySelectorAll('b'); let reached = 0; window.addEventListener('ping', () => reached++); main.dispatchEvent(new Event('ping', {bubbles: true})); main.innerHTML = ''; window instanceof EventTarget && reached === 1 && list.length === 1 && list[0].textContent === 'retained' && !list[0].isConnected && document.ownerDocument === null && document.textContent === null").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn event_target_install_accepts_bare_host_without_runtime_web_unit() {
        let mut engine = Engine::new();
        let global = engine.ctx().global_object();
        assert!(matches!(engine.ctx()
            .has_own_property_value(&global, &Value::str("EventTarget")), Ok(false)));
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let value = script(&mut engine, r#"
            const main = document.querySelector('main');
            let received;
            main.addEventListener('probe', event => received = event);
            const event = new Event('probe');
            main.dispatchEvent(event);
            main instanceof EventTarget && received === event &&
                event.target === main && event instanceof Event
        "#);
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn event_target_install_propagates_existing_getter_reference_error() {
        let mut engine = Engine::new();
        script(&mut engine, r#"
            globalThis.eventTargetGetterCalls = 0;
            Object.defineProperty(globalThis, 'EventTarget', {
                configurable: true,
                get() {
                    globalThis.eventTargetGetterCalls++;
                    throw new ReferenceError('existing EventTarget getter failure');
                }
            });
        "#);
        assert!(matches!(install(engine.ctx(), "<main></main>", 64), Err(InstallError::Global)));
        assert!(matches!(script(&mut engine, "eventTargetGetterCalls"), Value::Num(1.0)));
        // The failed getter must not be replaced by successful DOM installation.
        let value = script(&mut engine, r#"
            let error;
            try { globalThis.EventTarget; } catch (caught) { error = caught; }
            eventTargetGetterCalls === 2 && error instanceof ReferenceError &&
                error.message === 'existing EventTarget getter failure'
        "#);
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn deferred_event_subclasses_use_the_installed_dom_event_brand() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let value = engine
            .eval_value(
                r#"
                const main = document.querySelector('main');
                const seen = [];
                main.addEventListener('message', event => seen.push(event.data));
                main.addEventListener('custom', event => seen.push(event.detail));
                main.addEventListener('error', event => seen.push(event.message));
                const message = new MessageEvent('message', {data: 'message'});
                const custom = new CustomEvent('custom', {detail: 'custom'});
                const error = new ErrorEvent('error', {message: 'error'});
                main.dispatchEvent(message);
                main.dispatchEvent(custom);
                main.dispatchEvent(error);
                let rejected = false;
                try { main.dispatchEvent(Object.create(Event.prototype)); }
                catch (exception) { rejected = exception instanceof TypeError; }
                [message, custom, error].every(event => event instanceof Event && event.target === main) &&
                  seen.join(',') === 'message,custom,error' && rejected
                "#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn reactive_jobs_batch_dependencies_memos_and_cleanup() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var values = []; var cleaned = 0; var dispose; var setA; var setB; var choose; var setChoose; var total; __lumen.createRoot(d => { dispose = d; const a = __lumen.signal(1); const b = __lumen.signal(10); const c = __lumen.signal(true); setA = a[1]; setB = b[1]; choose = c[0]; setChoose = c[1]; total = __lumen.memo(() => a[0]() * 2); __lumen.effect(() => { __lumen.onCleanup(() => cleaned++); values.push(choose() ? a[0]() : b[0]()); }); });",
        );
        engine.ctx().drain_microtasks_for_host();
        let value = engine
            .eval_value("__lumen.batch(() => { setA(2); setA(3); }); total() === 6")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(value, Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        engine.eval_value("setChoose(false)").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        engine
            .eval_value("setA(4); setB(11)")
            .unwrap()
            .ok()
            .unwrap();
        engine.ctx().drain_microtasks_for_host();
        let value = engine
            .eval_value("dispose(); setB(12); values.join(',') === '1,3,10,11' && cleaned === 4")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn reactive_nested_owners_cleanup_once_and_stop_after_disposal() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var order=[]; var setValue; var disposeRoot; __lumen.createRoot(dispose => { disposeRoot=dispose; __lumen.onCleanup(()=>order.push('root-clean')); const state=__lumen.signal(0); setValue=state[1]; __lumen.effect(()=>{ const value=state[0](); __lumen.onCleanup(()=>order.push('outer-clean-'+value)); order.push('outer-'+value); }); __lumen.createRoot(()=>{ __lumen.onCleanup(()=>order.push('child-clean')); __lumen.effect(()=>order.push('child-run')); }); });",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(&mut engine, "order.join(',') === 'outer-0,child-run'"),
            Value::Bool(true)
        ));

        script(&mut engine, "setValue(1);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "order.join(',') === 'outer-0,child-run,outer-clean-0,outer-1'"
            ),
            Value::Bool(true)
        ));

        script(&mut engine, "disposeRoot(); setValue(2);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "order.join(',') === 'outer-0,child-run,outer-clean-0,outer-1,outer-clean-1,child-clean,root-clean'"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn reactive_batches_coalesce_and_flush_nested_effects_before_promises() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var order=[]; var setA; var setB; const a=__lumen.signal(0); const b=__lumen.signal(0); setA=a[1]; setB=b[1]; __lumen.effect(()=>{ const value=a[0](); order.push('a'+value); if(value===2) setB(2); }); __lumen.effect(()=>order.push('b'+b[0]()));",
        );
        engine.ctx().drain_microtasks_for_host();
        script(
            &mut engine,
            "order.length=0; __lumen.batch(()=>{setA(1);setA(2);}); Promise.resolve().then(()=>order.push('promise'));",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(&mut engine, "order.join(',') === 'a2,b2,promise'"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn reactive_effect_errors_dispose_failed_run_and_recover_through_boundary() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var disposeRoot; var setMode; var errors=[]; var order=[]; __lumen.createRoot(dispose=>{ disposeRoot=dispose; __lumen.errorBoundary(()=>{ const mode=__lumen.signal('ready'); setMode=mode[1]; __lumen.effect(()=>{ const value=mode[0](); __lumen.onCleanup(()=>order.push('cleanup-'+value)); if(value==='broken') throw 'effect failed'; order.push('run-'+value); }); },error=>errors.push(error)); });",
        );
        engine.ctx().drain_microtasks_for_host();
        script(&mut engine, "setMode('broken');");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "errors.join(',')==='effect failed' && order.join(',')==='run-ready,cleanup-ready,cleanup-broken'"
            ),
            Value::Bool(true)
        ));

        script(&mut engine, "setMode('recovered');");
        engine.ctx().drain_microtasks_for_host();
        script(&mut engine, "disposeRoot(); setMode('after-disposal');");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "order.join(',')==='run-ready,cleanup-ready,cleanup-broken,run-recovered,cleanup-recovered' && errors.length===1"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn nested_reactive_boundary_catches_errors_thrown_by_inner_handler() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var outerErrors=[]; __lumen.errorBoundary(()=>__lumen.errorBoundary(()=>__lumen.effect(()=>{throw 'source error';}),()=>{throw 'inner handler error';}),error=>outerErrors.push(error));",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(&mut engine, "outerErrors.join(',')==='inner handler error'"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn keyed_region_failure_keeps_prior_dom_and_disposes_partial_rows() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var main=document.querySelector('main'); var errors=[]; var cleaned=[]; var setRows; var disposeRoot; __lumen.createRoot(dispose=>{disposeRoot=dispose; const rows=__lumen.signal(['good']); setRows=rows[1]; __lumen.errorBoundary(()=>main.appendChild(__lumen.For({each:rows[0],children:item=>{__lumen.onCleanup(()=>cleaned.push(item)); if(item==='bad') throw 'row render failed'; const node=document.createElement('b'); node.textContent=item; return node;}})),error=>errors.push(error));});",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(&mut engine, "main.textContent==='good'"),
            Value::Bool(true)
        ));

        script(&mut engine, "setRows(['bad']);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "errors.join(',')==='row render failed' && main.textContent==='good' && cleaned.join(',')==='bad'"
            ),
            Value::Bool(true)
        ));

        script(&mut engine, "setRows([]);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "main.textContent==='' && cleaned.join(',')==='bad,good'"
            ),
            Value::Bool(true)
        ));
        script(&mut engine, "disposeRoot();");
        engine.ctx().drain_microtasks_for_host();
    }

    #[test]
    fn reactive_errors_reach_their_owner_boundary() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        script(
            &mut engine,
            "var caught = ''; __lumen.errorBoundary(() => { __lumen.effect(() => { throw 'boom'; }); }, e => { caught = e; });",
        );
        engine.ctx().drain_microtasks_for_host();
        let value = engine
            .eval_value("caught === 'boom'")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn native_templates_bind_only_touched_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app></div>", 128).unwrap();
        engine.eval_value("var template = __lumen.template('<section><span> </span><b>static</b></section>');").unwrap().ok().unwrap();
        assert_eq!(realm.wrapper_count(), 0);
        script(
            &mut engine,
            "var state = __lumen.signal('one'); var tree = __lumen.instantiate(template); var slot = __lumen.nodeAt(tree, [0,0]); __lumen.bindText(slot, state[0]); __lumen.bindAttribute(tree, 'class', () => state[0]()); document.getElementById('app').appendChild(tree);",
        );
        assert_eq!(realm.wrapper_count(), 3);
        engine.eval_value("state[1]('two')").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        let value = engine.eval_value("tree.textContent === 'twostatic' && tree.className === 'two' && slot === __lumen.nodeAt(tree,[0,0])").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn keyed_regions_preserve_rows_and_dispose_removed_owners() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var rows = __lumen.signal(['a','b']); var renders = 0; var cleanedRows = 0; var indexes = []; var main = document.querySelector('main'); main.appendChild(__lumen.For({each:rows[0],children:(item,index) => { renders++; indexes.push(index); __lumen.onCleanup(() => cleanedRows++); const node = document.createElement('b'); node.textContent = item; return node; }})); var firstRow = main.firstChild; rows[1](['b','a']);",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "main.textContent === 'ba' && main.childNodes[1] === firstRow && renders === 2 && indexes[0]() === 1 && indexes[1]() === 0"
            ),
            Value::Bool(true)
        ));
        script(&mut engine, "rows[1](['a']);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "main.textContent === 'a' && main.firstChild === firstRow && cleanedRows === 1"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn native_key_defaults_edit_inputs_and_respect_cancellation_and_limits() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<input maxlength=2><textarea>a</textarea>",
            128,
        )
        .unwrap();
        script(
            &mut engine,
            "var input = document.querySelector('input'); var textarea = document.querySelector('textarea'); var edits = 0; input.addEventListener('input',() => edits++); input.addEventListener('beforeinput',e => { if(e.data === 'q') e.preventDefault(); }); input.focus();",
        );
        let input = realm.focused_node().unwrap();
        for key in ["x", "y", "z", "Backspace", "q"] {
            realm
                .dispatch(
                    engine.ctx(),
                    input,
                    "keydown",
                    true,
                    true,
                    &[("key", Value::str(key))],
                )
                .unwrap();
        }
        assert!(matches!(
            script(&mut engine, "input.value === 'x' && edits === 3"),
            Value::Bool(true)
        ));
        script(&mut engine, "input.readOnly = true;");
        realm
            .dispatch(
                engine.ctx(),
                input,
                "keydown",
                true,
                true,
                &[("key", Value::str("a"))],
            )
            .unwrap();
        assert!(matches!(
            script(&mut engine, "input.value === 'x' && edits === 3"),
            Value::Bool(true)
        ));
        script(&mut engine, "textarea.focus();");
        // Parser/default text does not move the initial caret to the end.
        // Exercise both that initial position and an explicitly selected end.
        assert!(matches!(
            script(&mut engine,
                "textarea.value === 'a' && textarea.selectionStart === 0 && textarea.selectionEnd === 0"),
            Value::Bool(true)
        ));
        let textarea = realm.focused_node().unwrap();
        realm
            .dispatch(
                engine.ctx(),
                textarea,
                "keydown",
                true,
                true,
                &[("key", Value::str("Enter"))],
            )
            .unwrap();
        assert!(matches!(
            script(&mut engine,
                "textarea.value === '\\na' && textarea.selectionStart === 1 && textarea.selectionEnd === 1"),
            Value::Bool(true)
        ));
        script(&mut engine,
            "textarea.setSelectionRange(textarea.value.length,textarea.value.length);");
        realm
            .dispatch(
                engine.ctx(),
                textarea,
                "keydown",
                true,
                true,
                &[("key", Value::str("Enter"))],
            )
            .unwrap();
        realm
            .dispatch(
                engine.ctx(),
                textarea,
                "keydown",
                true,
                true,
                &[("key", Value::str("c")), ("ctrlKey", Value::Bool(true))],
            )
            .unwrap();
        let diagnostic = script(&mut engine,
            "JSON.stringify([textarea.value,textarea.selectionStart,textarea.selectionEnd])");
        if let Value::Str(actual) = diagnostic {
            assert_eq!(actual.as_str(), "[\"\\na\\n\",3,3]");
        } else {
            panic!("textarea diagnostic must be a string");
        }
    }

    #[test]
    fn mutation_observers_deliver_filtered_old_values_and_detached_subtrees() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><b>old</b></main>", 128).unwrap();
        script(
            &mut engine,
            "var main = document.querySelector('main'); var child = main.firstChild; var observed = []; var observer = new MutationObserver(records => observed.push(...records)); observer.observe(main,{subtree:true,attributes:true,attributeOldValue:true,attributeFilter:['title'],characterData:true,characterDataOldValue:true,childList:true}); child.setAttribute('title','one'); child.setAttribute('title','two'); child.setAttribute('class','ignored'); child.firstChild.data = 'new'; main.removeChild(child); child.setAttribute('title','detached'); var observerOrder = []; new MutationObserver(() => observerOrder.push('observer')).observe(main,{attributes:true}); main.setAttribute('title','schedule'); Promise.resolve().then(() => observerOrder.push('promise'));",
        );
        engine.collect_garbage();
        realm.with_session(|session| {
            session.document_mut().clear_mutations();
        });
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "observed.length === 6 && observed[0].oldValue === null && observed[1].oldValue === 'one' && observed[2].oldValue === 'old' && observed[3].removedNodes[0] === child && observed[4].oldValue === 'two' && observerOrder.join(',') === 'observer,promise'"
            ),
            Value::Bool(true)
        ));
        script(
            &mut engine,
            "child.setAttribute('title','after-delivery'); observer.disconnect(); main.setAttribute('title','disconnected');",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "observed.length === 6 && observer.takeRecords().length === 0"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn mutation_observer_take_records_does_not_call_callback() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(
            script(
                &mut engine,
                "var called = 0; var observer = new MutationObserver(() => called++); var main = document.querySelector('main'); observer.observe(main,{childList:true}); main.appendChild(document.createElement('b')); var records = observer.takeRecords(); records.length === 1 && records[0].addedNodes[0] === main.firstChild"
            ),
            Value::Bool(true)
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(&mut engine, "called === 0"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn bulk_replacement_records_keep_all_nodes_and_detached_subtrees() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><b>A</b><i>B</i></main>", 128).unwrap();
        assert!(matches!(
            script(
                &mut engine,
                "var main=document.querySelector('main'),a=main.firstChild,b=main.lastChild,records=[];var observer=new MutationObserver(r=>records.push(...r));observer.observe(main,{childList:true,subtree:true,attributes:true});main.innerHTML='<em>C</em><strong>D</strong>';a.setAttribute('title','detached');b.setAttribute('title','detached');true"
            ),
            Value::Bool(true)
        ));
        engine.collect_garbage();
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "records.length===3 && records[0].target===main && records[0].addedNodes.length===2 && records[0].addedNodes[0]===main.firstChild && records[0].addedNodes[1]===main.lastChild && records[0].removedNodes.length===2 && records[0].removedNodes[0]===a && records[0].removedNodes[1]===b && records[0].previousSibling===null && records[0].nextSibling===null && records[1].target===a && records[2].target===b"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            script(
                &mut engine,
                "records=[];var fragment=document.createDocumentFragment();fragment.append(document.createElement('u'),document.createElement('s'));var fo=new MutationObserver(()=>{});fo.observe(fragment,{childList:true});main.replaceChildren(fragment);var fr=fo.takeRecords(),mr=observer.takeRecords();fr.length===1 && fr[0].removedNodes.length===2 && fr[0].addedNodes.length===0 && mr.length===1 && mr[0].addedNodes.length===2 && mr[0].removedNodes.length===2"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn programmatic_focus_events_are_trusted_and_release_native_receivers() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=first><input id=second>", 128).unwrap();
        assert!(matches!(
            script(
                &mut engine,
                r#"
            var first = document.getElementById('first');
            var second = document.getElementById('second');
            var order = [];
            for (const type of ['focus', 'focusin', 'blur', 'focusout']) {
                document.addEventListener(type, e => { if (!(e instanceof FocusEvent)) throw new Error('focus event interface'); order.push(type + ':' + e.isTrusted); }, true);
            }
            first.focus(); second.focus(); second.blur();
            order.join(',') === 'focus:true,focusin:true,blur:true,focusout:true,focus:true,focusin:true,blur:true,focusout:true'
        "#
            ),
            Value::Bool(true)
        ));
        assert!(!realm.has_transient_user_activation());
        assert!(!realm.has_been_active());
        assert!(matches!(
            script(
                &mut engine,
                r#"
            var other = document.implementation.createHTMLDocument('other');
            first.addEventListener('focus', () => other.adoptNode(first), {once:true});
            first.focus();
            first.ownerDocument === other && document.activeElement === document.body
        "#
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn focus_input_properties_and_keyboard_events_follow_active_element() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<main><input id=first><button id=second></button></main>",
            128,
        )
        .unwrap();
        assert!(matches!(
            script(
                &mut engine,
                "var first = document.getElementById('first'); var second = document.getElementById('second'); var focusOrder = []; document.querySelector('main').addEventListener('focusin', e => focusOrder.push(e.target.id)); first.value = 'typed'; first.checked = true; first.focus(); second.disabled = true; second.focus(); document.activeElement === first && first.value === 'typed' && first.checked && focusOrder.join(',') === 'first'"
            ),
            Value::Bool(true)
        ));
        let focused = realm.focused_node().unwrap();
        script(
            &mut engine,
            "first.addEventListener('keydown', e => { first.value += e.key; e.preventDefault(); });",
        );
        assert!(!realm
            .dispatch(
                engine.ctx(),
                focused,
                "keydown",
                true,
                true,
                &[("key", Value::str("x"))]
            )
            .unwrap());
        assert!(matches!(
            script(
                &mut engine,
                "second.disabled = false; second.focus(); second.blur(); first.value === 'typedx' && document.activeElement === document.body && focusOrder.join(',') === 'first,second'"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.focused_node(), None);
    }

    #[test]
    fn interaction_pseudos_follow_sparse_realm_state_across_detach_and_reinsert() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<main id=parent><button id=target tabindex=0></button></main>",
            64,
        )
        .unwrap();
        assert!(matches!(
            script(
                &mut engine,
                "typeof document.hasFocus === 'function' && document.hasFocus()"
            ),
            Value::Bool(true)
        ));
        let (parent, target) = realm.with_session(|session| {
            let document = session.document();
            (
                selector::query_selector(document, document.root(), "#parent")
                    .unwrap()
                    .unwrap(),
                selector::query_selector(document, document.root(), "#target")
                    .unwrap()
                    .unwrap(),
            )
        });
        let matches = |realm: &DomRealm, query: &str| {
            realm.with_session(|session| {
                selector::query_selector(session.document(), session.document().root(), query)
                    .unwrap()
                    .is_some()
            })
        };

        assert!(!matches(&realm, "#target:focus"));
        assert!(!matches(&realm, "main:focus-within"));
        assert!(!matches(&realm, "#target:hover"));
        assert!(!matches(&realm, "#target:active"));

        realm.focus(engine.ctx(), Some(target)).unwrap();
        assert!(matches(&realm, "#target:focus"));
        assert!(matches(&realm, "main:focus-within"));
        assert!(matches(&realm, "#target:focus-visible"));

        realm.note_pointer_modality();
        realm.focus_from_pointer(engine.ctx(), Some(target)).unwrap();
        assert!(matches(&realm, "#target:focus"));
        assert!(!matches(&realm, "#target:focus-visible"));
        realm.note_keyboard_modality();
        assert!(matches(&realm, "#target:focus-visible"));

        realm.publish_hover_target(Some(target));
        realm.publish_active_targets(Some(target), None);
        assert!(matches(&realm, "#target:hover"));
        assert!(matches(&realm, "main:hover"));
        assert!(matches(&realm, "#target:active"));
        assert!(matches(&realm, "main:active"));

        assert!(matches!(
            script(
                &mut engine,
                "const target=document.getElementById('target'); const parent=document.getElementById('parent'); target.remove(); parent.append(target); target.isConnected"
            ),
            Value::Bool(true)
        ));
        assert!(!matches(&realm, "#target:focus"));
        assert!(!matches(&realm, "main:focus-within"));
        assert!(!matches(&realm, "#target:hover"));
        assert!(!matches(&realm, "#target:active"));

        // Republishing the same retained NodeId after reattachment must update
        // the shared selector snapshot, even though the realm's old target was
        // identical before the detach.
        realm.publish_hover_target(Some(target));
        assert!(matches(&realm, "#target:hover"));
        assert!(matches(&realm, "main:hover"));
        assert!(!matches(&realm, "#target:focus"));

        // Pointer modality and ordinary CSS interaction selectors do not make
        // a detached-and-reinserted control focused again.
        let _ = parent;
    }

    #[test]
    fn focusability_applies_disabled_fieldset_and_first_legend_rules_to_controls_only() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<fieldset id=disabled-fieldset disabled tabindex=0><legend><input id=legend-input></legend><input id=fieldset-disabled><div id=generic-disabled disabled tabindex=0>generic</div><a id=disabled-link href='#' disabled>link</a></fieldset><input id=own-disabled disabled><input id=outside>",
            128,
        )
        .unwrap();
        let node = |id: &str| {
            realm.with_session(|session| {
                selector::query_selector(
                    session.document(),
                    session.document().root(),
                    &format!("#{id}"),
                )
                .unwrap()
                .unwrap()
            })
        };
        let legend_input = node("legend-input");
        let generic_disabled = node("generic-disabled");
        let disabled_link = node("disabled-link");
        let outside = node("outside");
        let disabled_fieldset = node("disabled-fieldset");
        let fieldset_disabled = node("fieldset-disabled");
        let own_disabled = node("own-disabled");

        realm.focus_next(engine.ctx(), false).unwrap();
        assert_eq!(realm.focused_node(), Some(legend_input));
        realm
            .dispatch(
                engine.ctx(),
                legend_input,
                "keydown",
                true,
                true,
                &[("key", Value::str("x"))],
            )
            .unwrap();
        assert!(matches!(
            script(&mut engine, "document.activeElement.value === 'x'"),
            Value::Bool(true)
        ));

        realm.focus_next(engine.ctx(), false).unwrap();
        assert_eq!(realm.focused_node(), Some(generic_disabled));
        realm.focus_next(engine.ctx(), false).unwrap();
        assert_eq!(realm.focused_node(), Some(disabled_link));
        realm.focus_next(engine.ctx(), false).unwrap();
        assert_eq!(realm.focused_node(), Some(outside));

        realm.focus(engine.ctx(), Some(disabled_fieldset)).unwrap();
        assert_eq!(realm.focused_node(), Some(outside));
        realm.focus(engine.ctx(), Some(fieldset_disabled)).unwrap();
        assert_eq!(realm.focused_node(), Some(outside));
        realm.focus(engine.ctx(), Some(own_disabled)).unwrap();
        assert_eq!(realm.focused_node(), Some(outside));
    }

    #[test]
    fn web_targets_capture_bubble_and_preserve_abort_signals() {
        let mut engine = Engine::new();
        script(&mut engine, "globalThis.performance = {now:() => 0};");
        script(
            &mut engine,
            include_str!("../../lumen-web/src/js/events.js"),
        );
        let value = script(
            &mut engine,
            "const root = new EventTarget(); const leaf = new EventTarget(); leaf.parentNode = root; const order = []; root.addEventListener('ping', e => order.push('capture'+e.eventPhase), true); leaf.addEventListener('ping', e => { order.push('target'+e.eventPhase); e.preventDefault(); }, {passive:true}); root.addEventListener('ping', e => order.push('bubble'+e.eventPhase)); const event = new Event('ping',{bubbles:true,cancelable:true}); const dispatched = leaf.dispatchEvent(event); const controller = new AbortController(); let aborts = 0; controller.signal.addEventListener('abort',() => aborts++); controller.abort(); dispatched && !event.defaultPrevented && event.currentTarget === null && event.eventPhase === 0 && order.join(',') === 'capture1,target2,bubble3' && aborts === 1 && controller.signal.aborted",
        );
        assert!(matches!(value, Value::Bool(true)));
        install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(
            script(
                &mut engine,
                "let domError; try { document.querySelector('main').appendChild(document); } catch (error) { domError = error; } domError instanceof DOMException && domError.name === 'HierarchyRequestError'"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn child_slots_and_show_replace_owned_regions() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "var main = document.querySelector('main'); var marker = document.createComment(''); main.appendChild(marker); var child = __lumen.signal('first'); __lumen.bindChild(marker,child[0]); var visible = __lumen.signal(false); main.appendChild(__lumen.Show({when:visible[0],children:() => 'shown',fallback:'hidden'})); var replacement = document.createElement('b'); replacement.textContent = 'node'; child[1]([replacement,'tail']); visible[1](true);",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "main.textContent === 'nodetailshown' && main.firstChild === replacement"
            ),
            Value::Bool(true)
        ));
        script(&mut engine, "child[1](null); visible[1](false);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "main.textContent === 'hidden' && replacement.parentNode === null"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn compiled_jsx_slots_update_native_dom() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state = __lumen.signal('one'); globalThis.updateSlot = state[1]; globalThis.compiledTree = <div title={state[0]()}><span>{state[0]()}</span><b>static</b></div>; document.querySelector('main').appendChild(compiledTree);", "app.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        script(&mut engine, "updateSlot('two');");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "compiledTree.getAttribute('title') === 'two' && compiledTree.textContent === 'twostatic'"
            ),
            Value::Bool(true)
        ));
        let mut static_engine = Engine::new();
        let static_realm = install(
            static_engine.ctx(),
            "<main><div title=two><span>two</span><b>static</b></div></main>",
            128,
        )
        .unwrap();
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let render = |realm: &Rc<DomRealm>| {
            realm.with_session(|session| {
                render_with_font(
                    session.display_list(160, 80, &font).unwrap(),
                    160,
                    80,
                    1.0,
                    false,
                    &font,
                )
                .unwrap()
                .pixels
            })
        };
        assert_eq!(render(&realm), render(&static_realm));
    }

    #[test]
    fn compiled_jsx_object_styles_preserve_units_and_replace_removed_properties() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state=__lumen.signal({width:0,height:10,opacity:0,zIndex:2,'--Size':0}); globalThis.updateStyles=state[1]; globalThis.styled=<div style={state[0]()}/>; document.querySelector('main').appendChild(styled);", "style.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        assert!(matches!(
            script(
                &mut engine,
                "styled.style.getPropertyValue('width')==='0px' && styled.style.getPropertyValue('height')==='10px' && styled.style.getPropertyValue('opacity')==='0' && styled.style.getPropertyValue('z-index')==='2' && styled.style.getPropertyValue('--Size')==='0'"
            ),
            Value::Bool(true)
        ));
        script(
            &mut engine,
            "updateStyles({width:4,fontSize:12,opacity:1,'--Size':2});",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "styled.style.getPropertyValue('width')==='4px' && styled.style.getPropertyValue('font-size')==='12px' && styled.style.getPropertyValue('opacity')==='1' && styled.style.getPropertyValue('--Size')==='2' && styled.style.getPropertyValue('height')==='' && styled.style.getPropertyValue('z-index')===''"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn compiled_mixed_child_slots_keep_static_siblings() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state = __lumen.signal(2); globalThis.updateMixed = state[1]; globalThis.mixed = <div>before {state[0]()}<span>{state[0]()}</span>{[state[0](),3]}</div>; document.querySelector('main').appendChild(mixed);", "mixed.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        script(
            &mut engine,
            "var staticSpan = mixed.querySelector('span'); updateMixed(4);",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            script(
                &mut engine,
                "mixed.textContent === 'before 4443' && mixed.querySelector('span') === staticSpan"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn jsx_runtime_creates_dom_for_components_fragments_and_events() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let value = engine.eval_value("const {jsx,jsxs,Fragment} = __lumen; let clicked = 0; function Button(props) { return jsx('button', {className:'ok', onClick:() => clicked++, children:props.label}); } const tree = jsxs('section', {style:{width:20,height:10,backgroundColor:'red'}, children:[jsx(Button,{label:'press'}), jsx(Fragment,{children:['a','b']})]}); document.querySelector('main').appendChild(tree); tree.firstChild.dispatchEvent(new Event('click')); clicked === 1 && tree.textContent === 'pressab' && tree.firstChild instanceof HTMLElement && tree.style.width === '20px'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn js_mutates_the_render_document_with_stable_wrappers() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<!doctype html><div id=app>old</div>", 128).unwrap();
        let result = engine.eval_value("const app = document.querySelector('#app'); const same = app === document.body.firstChild; app.innerHTML = '<span>new</span>'; const span = app.firstChild; span.setAttribute('class', 'ok'); same && app === document.querySelector('#app') && span === app.querySelector('span') && span.matches('.ok') && document.documentElement.nodeName === 'HTML' && document.body.parentNode === document.documentElement && document.documentElement.parentNode === document && app.innerHTML === '<span class=\"ok\">new</span>'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let app =
                selector::query_selector(session.document(), session.document().root(), "#app")
                    .unwrap()
                    .unwrap();
            assert_eq!(
                html::inner_html(session.document(), app).unwrap(),
                "<span class=\"ok\">new</span>"
            );
        });
    }

    #[test]
    fn node_moves_clone_and_text_content_follow_dom_identity() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><p>A</p><p>B</p></main>", 128).unwrap();
        let result = engine.eval_value("const main = document.querySelector('main'); const first = main.firstChild; const second = main.lastChild; const copy = first.cloneNode(true); const cloned = copy !== first && copy.textContent === 'A'; const moved = main.insertBefore(second, first) === second && main.firstChild === second && first.previousSibling === second; const removed = main.removeChild(second) === second && second.parentNode === null; main.appendChild(second); first.textContent = 'new'; cloned && moved && removed && main.textContent === 'newB' && first.firstChild.textContent === 'new'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let main =
                selector::query_selector(session.document(), session.document().root(), "main")
                    .unwrap()
                    .unwrap();
            let mut text = String::new();
            session
                .document()
                .append_descendant_text(main, &mut text)
                .unwrap();
            assert_eq!(text, "newB");
        });
    }

    #[test]
    fn template_content_identity_and_inner_html() {
        let mut engine = Engine::new();
        let _realm = install(
            engine.ctx(),
            "<template><b>A</b><template><i>B</i></template></template>",
            128,
        )
        .unwrap();
        let result = engine.eval_value("const t=document.querySelector('template'); const c=t.content; const copy=t.cloneNode(true); const ok=t instanceof HTMLTemplateElement && c instanceof DocumentFragment && c===t.content && c.parentNode===null && t.childNodes.length===0 && t.textContent==='' && document.querySelector('b')===null && c.querySelector('b').textContent==='A' && copy.content!==c && copy.innerHTML===t.innerHTML; t.innerHTML='<i>C</i>'; ok && t.content===c && c.firstChild.textContent==='C' && t.childNodes.length===0").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn js_style_mutation_changes_rendered_pixels() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<div style='width:20px;height:20px;background:red'></div>",
            64,
        )
        .unwrap();
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let before = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        engine.eval_value("document.querySelector('div').setAttribute('style', 'width:20px;height:20px;background:blue')").unwrap().ok().unwrap();
        let after = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        assert_ne!(before.pixels, after.pixels);
        assert_eq!(after.pixels.len(), 32 * 32 * 4);
    }

    #[test]
    fn replacing_stylesheet_rebuilds_cached_css() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<style>div{width:20px;height:20px;background:red}</style><div></div>",
            128,
        )
        .unwrap();
        realm.with_session(|session| {
            let doc = session.document();
            let head = selector::query_selector(doc, doc.root(), "head")
                .unwrap()
                .unwrap();
            let style = selector::query_selector(doc, doc.root(), "style")
                .unwrap()
                .unwrap();
            assert_eq!(doc.parent(style).unwrap(), Some(head));
        });
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let before = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        engine.eval_value("document.querySelector('head').innerHTML = '<style>div{width:20px;height:20px;background:blue}</style>'").unwrap().ok().unwrap();
        assert!(realm
            .session
            .borrow()
            .document()
            .mutations()
            .iter()
            .any(|m| matches!(
                m.kind,
                lumen_html::MutationKind::Tree {
                    styles_changed: true,
                    ..
                }
            )));
        realm.with_session(|session| {
            let head =
                selector::query_selector(session.document(), session.document().root(), "head")
                    .unwrap()
                    .unwrap();
            assert_eq!(
                html::inner_html(session.document(), head).unwrap(),
                "<style>div{width:20px;height:20px;background:blue}</style>"
            );
        });
        let after = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        let inside = (10 * 32 + 10) * 4;
        assert_ne!(
            &before.pixels[inside..inside + 4],
            &after.pixels[inside..inside + 4]
        );
    }

    #[test]
    fn document_lookup_and_character_data_use_shared_nodes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div id=app>x</div>", 64).unwrap();
        let result = engine.eval_value("const app = document.getElementById('app'); const text = app.firstChild; const comment = document.createComment('note'); app.appendChild(comment); text.nodeValue = 'y'; app.className = 'active'; app === document.querySelector('#app') && app.ownerDocument === document && window.document === document && app.className === 'active' && text.nodeValue === 'y' && app.textContent === 'y' && comment.nodeType === 8 && comment.nodeValue === 'note'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn repeated_inner_html_reuses_arena_slots_and_keeps_live_detached_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app><span>old</span></div>", 32).unwrap();
        let result = engine.eval_value("const app = document.getElementById('app'); const old = app.firstChild; for (let i = 0; i < 100; i++) app.innerHTML = '<b>new</b>'; old.textContent === 'old' && old.parentNode === null && app.firstChild.nodeName === 'B'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        assert!(realm.with_session(|session| session.document().node_count()) <= 12);
    }

    #[test]
    fn namespace_creation_keeps_svg_casing() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = engine.eval_value("const svg = document.createElementNS('http://www.w3.org/2000/svg', 'linearGradient'); document.querySelector('main').appendChild(svg); svg.nodeName === 'linearGradient' && document.createElementNS('http://www.w3.org/1999/xhtml', 'DIV').nodeName === 'DIV'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn frame_reaps_detached_nodes_after_wrapper_collection() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app><span>old</span></div>", 32).unwrap();
        engine.eval_value("const app = document.getElementById('app'); (function () { const old = app.firstChild; app.innerHTML = '<b>new</b>'; })()").unwrap().ok().unwrap();
        engine.collect_garbage();
        let live = realm.with_session(|session| session.document().node_count());
        assert!(live <= 8, "{live} live nodes");
    }

    #[test]
    fn document_title_is_live_contextual_and_survives_document_cloning() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<!doctype html><html><head></head><body></body></html>",
            256,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"(() => {
            const failures = [];
            const check = (condition, label) => { if (!condition) failures.push(label); };
            check(document.title === '', 'initial title');
            let conversions = 0;
            document.title = { toString() { conversions++; return '  first\t title\n '; } };
            const title = document.head.querySelector('title');
            check(conversions === 1 && document.title === 'first title' &&
                title.textContent === '  first\t title\n ', 'setter conversion and normalization');
            const later = document.createElement('title');
            later.textContent = 'later';
            document.head.appendChild(later);
            title.textContent = '  live\n title  ';
            check(document.title === 'live title', 'first title live text');
            const nested = document.createElement('span');
            nested.textContent = 'excluded descendant';
            title.appendChild(nested);
            check(document.title === 'live title', 'direct child text only');
            const clone = document.cloneNode(true);
            check(clone.title === 'live title', 'document clone title');
            clone.title = 'copy';
            check(clone.title === 'copy' && document.title === 'live title', 'clone independent setter');
            title.remove();
            check(document.title === 'later', 'tree order after removal');
            document.title = null;
            check(document.title === 'null', 'DOMString null conversion');
            const withoutHead = document.implementation.createHTMLDocument('before');
            withoutHead.head.remove();
            withoutHead.title = 'ignored';
            check(withoutHead.title === '', 'no head no title no mutation');
            const parser = new DOMParser();
            const svg = parser.parseFromString('<svg xmlns="http://www.w3.org/2000/svg"><g><title>nested</title></g></svg>', 'image/svg+xml');
            svg.title = '  SVG\n title ';
            check(svg.title === 'SVG title' && svg.documentElement.firstChild.localName === 'title' &&
                svg.documentElement.firstChild.namespaceURI === 'http://www.w3.org/2000/svg', 'SVG title first child');
            const xml = parser.parseFromString('<root><title>foreign</title></root>', 'application/xml');
            xml.title = 'ignored';
            check(xml.title === '' && xml.documentElement.firstChild.textContent === 'foreign', 'unrelated XML namespace');
            return failures.join('|');
        })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("document title diagnostics must be a string")
        };
        assert!(
            failures.is_empty(),
            "document title contract failed: {failures}"
        );
    }

    #[test]
    fn html_div_and_br_interfaces_preserve_class_identity_and_reflection() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div id='box'><br></div><span></span>", 128).unwrap();
        let result = script(
            &mut engine,
            r#"(() => {
            const div = document.getElementById('box');
            const br = div.firstChild;
            const span = document.querySelector('span');
            const failures = [];
            const check = (condition, label) => { if (!condition) failures.push(label); };
            check(div instanceof HTMLDivElement && div instanceof HTMLElement &&
                div.constructor === HTMLDivElement && !(span instanceof HTMLDivElement), 'div-interface');
            check(br instanceof HTMLBRElement && br instanceof HTMLElement &&
                !(div instanceof HTMLBRElement), 'br-interface');
            check(Object.getPrototypeOf(HTMLDivElement.prototype) === HTMLElement.prototype &&
                Object.getPrototypeOf(HTMLBRElement.prototype) === HTMLElement.prototype, 'interface-inheritance');
            check(div.align === '' && br.clear === '', 'initial-reflection');
            div.align = 42;
            br.clear = 'all';
            check(div.getAttribute('align') === '42' && br.getAttribute('clear') === 'all', 'setter-reflection');
            div.setAttribute('align', 'left');
            br.setAttribute('clear', 'right');
            check(div.align === 'left' && br.clear === 'right', 'attribute-reflection');
            const cloned = div.cloneNode(true);
            check(cloned !== div && cloned instanceof HTMLDivElement &&
                cloned.firstChild instanceof HTMLBRElement && cloned.align === 'left' &&
                cloned.firstChild.clear === 'right', 'clone-interfaces');
            const prefixed = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:div');
            const prefixedBr = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:br');
            const foreign = document.createElementNS('urn:foreign', 'p:div');
            const uppercase = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:DIV');
            check(prefixed instanceof HTMLDivElement && prefixed.cloneNode() instanceof HTMLDivElement &&
                prefixed.localName === 'div' && prefixed.prefix === 'p' &&
                prefixedBr instanceof HTMLBRElement && prefixedBr.cloneNode() instanceof HTMLBRElement &&
                !(foreign instanceof HTMLDivElement) && !(uppercase instanceof HTMLDivElement),
                'namespace-local-name-interfaces');
            return failures.join('|');
        })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("HTML interface diagnostics must be a string");
        };
        assert!(failures.is_empty(), "HTML interfaces failed: {failures}");
    }

    #[test]
    fn html_leaf_interfaces_keep_distinct_brands_for_creation_and_cloning() {
        let mut engine = Engine::new();
        install(engine.ctx(), "", 256).unwrap();
        let result = script(
            &mut engine,
            r#"(() => {
            const failures = [];
            const check = (condition, label) => { if (!condition) failures.push(label); };
            const cases = [
                ['button', HTMLButtonElement], ['caption', HTMLTableCaptionElement],
                ['col', HTMLTableColElement], ['colgroup', HTMLTableColElement],
                ['data', HTMLDataElement], ['datalist', HTMLDataListElement],
                ['dialog', HTMLDialogElement], ['del', HTMLModElement], ['ins', HTMLModElement],
                ['dir', HTMLDirectoryElement], ['dl', HTMLDListElement], ['embed', HTMLEmbedElement],
                ['fieldset', HTMLFieldSetElement], ['font', HTMLFontElement],
                ['frame', HTMLFrameElement], ['frameset', HTMLFrameSetElement],
                ['h1', HTMLHeadingElement], ['h6', HTMLHeadingElement], ['hr', HTMLHRElement],
                ['label', HTMLLabelElement], ['legend', HTMLLegendElement], ['li', HTMLLIElement],
                ['map', HTMLMapElement], ['meta', HTMLMetaElement], ['meter', HTMLMeterElement],
                ['object', HTMLObjectElement], ['ol', HTMLOListElement],
                ['optgroup', HTMLOptGroupElement], ['output', HTMLOutputElement],
                ['p', HTMLParagraphElement], ['param', HTMLParamElement], ['pre', HTMLPreElement],
                ['listing', HTMLPreElement], ['xmp', HTMLPreElement],
                ['progress', HTMLProgressElement], ['blockquote', HTMLQuoteElement],
                ['q', HTMLQuoteElement], ['source', HTMLSourceElement], ['span', HTMLSpanElement],
                ['table', HTMLTableElement], ['tbody', HTMLTableSectionElement],
                ['tfoot', HTMLTableSectionElement], ['thead', HTMLTableSectionElement],
                ['td', HTMLTableCellElement], ['th', HTMLTableCellElement],
                ['tr', HTMLTableRowElement], ['time', HTMLTimeElement],
                ['track', HTMLTrackElement], ['ul', HTMLUListElement],
            ];
            for (let index = 0; index < cases.length; index++) {
                const name = cases[index][0];
                const ctor = cases[index][1];
                const element = document.createElement(name);
                const clone = element.cloneNode(false);
                check(element instanceof ctor && clone instanceof ctor &&
                    element instanceof HTMLElement && element.constructor === ctor,
                    'brand:' + name);
                check(Object.getPrototypeOf(ctor.prototype) === HTMLElement.prototype,
                    'prototype:' + name);
            }
            const unknown = document.createElement('unknown');
            const legacyUnknown = document.createElement('spacer');
            const custom = document.createElement('x-widget');
            check(unknown instanceof HTMLUnknownElement && unknown instanceof HTMLElement,
                'unknown-brand');
            check(legacyUnknown instanceof HTMLUnknownElement && legacyUnknown instanceof HTMLElement,
                'legacy-unknown-brand');
            check(custom instanceof HTMLElement && !(custom instanceof HTMLUnknownElement),
                'custom-name-brand');
            const prefixed = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:button');
            const foreign = document.createElementNS('urn:foreign', 'p:button');
            const uppercase = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:BUTTON');
            check(prefixed instanceof HTMLButtonElement && prefixed.cloneNode() instanceof HTMLButtonElement &&
                !(foreign instanceof HTMLButtonElement) && !(uppercase instanceof HTMLButtonElement),
                'namespace-and-qname-dispatch');
            return failures.join('|');
        })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("HTML leaf interface diagnostics must be a string");
        };
        assert!(
            failures.is_empty(),
            "HTML leaf interfaces failed: {failures}"
        );
    }

    #[test]
    fn id_lookup_is_scoped_live_and_shared_by_fragment_and_shadow() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<main id='shared'><i id=''></i></main><section></section>",
            128,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"(() => {
            const failures = [];
            const check = (condition, label) => { if (!condition) failures.push(label); };
            const fragment = document.createDocumentFragment();
            const first = document.createElement('span');
            const second = document.createElement('b');
            first.id = second.id = 'shared';
            fragment.appendChild(first);
            fragment.appendChild(second);
            check(fragment.getElementById('shared') === first, 'fragment-first-match');
            fragment.appendChild(first);
            check(fragment.getElementById('shared') === second, 'fragment-live-order');
            second.id = 'renamed';
            check(fragment.getElementById('shared') === first, 'fragment-live-id');
            check(document.getElementById('shared').localName === 'main', 'document-scope');
            check(document.getElementById('') === null && fragment.getElementById('') === null, 'empty-id');
            const shadow = document.querySelector('section').attachShadow({mode:'open'});
            const shadowChild = document.createElement('em');
            shadowChild.id = 'shared';
            shadow.appendChild(shadowChild);
            check(shadow.getElementById('shared') === shadowChild, 'shadow-scope');
            check(shadow.getElementById === DocumentFragment.prototype.getElementById, 'inherited-shared-method');
            let conversions = 0;
            check(fragment.getElementById({toString() { conversions++; return 'shared'; }}) === first &&
                conversions === 1, 'domstring-coercion');
            const nullId = document.createElement('i');
            nullId.id = 'null';
            fragment.appendChild(nullId);
            check(fragment.getElementById(null) === nullId, 'null-domstring');
            return failures.join('|');
        })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("ID lookup diagnostics must be a string");
        };
        assert!(failures.is_empty(), "ID lookup failed: {failures}");
    }

    #[test]
    fn character_data_uses_webidl_unsigned_long_and_null_to_empty_string() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = script(
            &mut engine,
            r#"(() => {
                const failures = [];
                for (const node of [document.createTextNode('abcdef'), document.createComment('abcdef')]) {
                    const check = (condition, label) => { if (!condition) failures.push(label); };
                    check(node.substringData(4294967297, -1) === 'bcdef', 'wrapped-offset-and-count');
                    check(node.substringData(1.9, 2.9) === 'bc', 'fractional-offset-and-count');
                    check(node.substringData(Infinity, NaN) === '', 'nonfinite-conversion');
                    for (const mutate of [
                        () => node.substringData(-1, 1),
                        () => node.insertData(-1, 'x'),
                        () => node.deleteData(-1, 1),
                        () => node.replaceData(-1, 1, 'x')
                    ]) {
                        let rejected = false;
                        try { mutate(); } catch (error) { rejected = error.name === 'IndexSizeError'; }
                        check(rejected && node.data === 'abcdef', 'invalid-offset-no-mutation');
                    }
                    node.insertData(4294967297, 'X');
                    check(node.data === 'aXbcdef', 'insert-wrapped-offset');
                    node.deleteData(2, -1);
                    check(node.data === 'aX', 'delete-wrapped-count');
                    node.replaceData(1, -1, 'Y');
                    check(node.data === 'aY', 'replace-wrapped-count');
                    node.data = null;
                    check(node.data === '' && node.length === 0, 'null-data-is-empty');
                    node.data = undefined;
                    check(node.data === 'undefined', 'undefined-data-is-string');
                    node.data = {toString() { return 'converted'; }};
                    check(node.data === 'converted', 'object-data-string-coercion');
                    node.data = '';
                    node.appendData(null);
                    check(node.data === 'null', 'append-null-is-string');
                }
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("CharacterData diagnostics must be a string");
        };
        assert!(
            failures.is_empty(),
            "CharacterData conversion failed: {failures}"
        );
    }

    #[test]
    fn node_insertion_adopts_foreign_nodes_and_preserves_identity_and_failure_atomicity() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(
            &mut engine,
            "globalThis.performance = {now:() => 0}; const NativeEvent = Event;",
        );
        script(
            &mut engine,
            include_str!("../../lumen-web/src/js/events.js"),
        );
        let result = script(
            &mut engine,
            r#"
            const donorDocument = new DOMParser().parseFromString('<section><i></i></section>', 'text/html');
            const target = document.querySelector('main');
            const attached = donorDocument.querySelector('i');
            let events = 0;
            attached.addEventListener('kept-listener', () => events++);
            const appended = target.appendChild(attached);
            attached.dispatchEvent(new NativeEvent('kept-listener'));
            const orphan = donorDocument.createElement('b');
            const inserted = target.insertBefore(orphan, attached);
            const fragment = donorDocument.createDocumentFragment();
            const text = donorDocument.createTextNode('text');
            fragment.appendChild(text);
            const returnedFragment = target.appendChild(fragment);
            const valid = appended === attached && inserted === orphan &&
                returnedFragment === fragment && fragment.childNodes.length === 0 &&
                fragment.ownerDocument === document && text.ownerDocument === document &&
                text.parentNode === target && orphan.ownerDocument === document &&
                attached.ownerDocument === document && target.firstChild === orphan && events === 1;

            const donor = donorDocument.querySelector('section');
            const unattached = donorDocument.createElement('em');
            let documentRejected = false, hierarchyRejected = false, referenceRejected = false;
            let parentRejected = false, referencePrecedesNodeKind = false;
            try { target.appendChild(donorDocument); }
            catch (error) { documentRejected = error.name === 'HierarchyRequestError' && error.code === 3; }
            try { document.appendChild(donor); }
            catch (error) { hierarchyRejected = error.name === 'HierarchyRequestError'; }
            try { target.insertBefore(unattached, donorDocument.body); }
            catch (error) { referenceRejected = error.name === 'NotFoundError'; }
            try { text.insertBefore(unattached, donorDocument.body); }
            catch (error) { parentRejected = error.name === 'HierarchyRequestError'; }
            try { target.insertBefore(donorDocument, donorDocument.body); }
            catch (error) { referencePrecedesNodeKind = error.name === 'NotFoundError'; }
            [
                !valid && 'identity-and-adoption',
                !documentRejected && 'document-rejected',
                !hierarchyRejected && 'hierarchy-rejected',
                !referenceRejected && 'reference-rejected',
                !parentRejected && 'parent-rejected',
                !referencePrecedesNodeKind && 'reference-precedes-node-kind',
                donor.parentNode !== donorDocument.body && 'donor-parent',
                donor.ownerDocument !== donorDocument && 'donor-owner',
                unattached.parentNode !== null && 'orphan-parent',
                unattached.ownerDocument !== donorDocument && 'orphan-owner'
            ].filter(Boolean).join('|')
        "#,
        );
        let Value::Str(failures) = result else {
            panic!("cross-document insertion diagnostics must be a string");
        };
        assert!(
            failures.is_empty(),
            "cross-document insertion failed: {failures}"
        );
    }

    #[test]
    fn node_insertion_revalidates_after_focus_callbacks_before_adoption() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><section><input></section></main>", 128).unwrap();
        let result = script(
            &mut engine,
            r#"
            const targetDocument = new DOMParser().parseFromString('<main><b></b></main>', 'text/html');
            const target = targetDocument.querySelector('main');
            const reference = target.firstChild;
            const donor = document.querySelector('section');
            const input = donor.firstChild;
            let notified = false;
            input.addEventListener('blur', () => { notified = true; reference.remove(); });
            input.focus();
            const focused = document.activeElement === input;
            let rejected = false;
            try { target.insertBefore(donor, reference); }
            catch (error) { rejected = error.name === 'NotFoundError'; }
            focused && notified && rejected && donor.ownerDocument === document &&
                donor.parentNode === document.querySelector('main') &&
                input.ownerDocument === document && reference.parentNode === null &&
                target.childNodes.length === 0
        "#,
        );
        assert!(
            matches!(result, Value::Bool(true)),
            "focus callback changed adoption failure semantics"
        );
    }

    #[test]
    fn document_adopt_node_rehomes_existing_wrappers_and_event_listeners() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
            var sourceDocument = new DOMParser().parseFromString('<section id="root"><b id="child">kept</b></section>', 'text/html');
            var source = sourceDocument.querySelector('#root');
            var child = source.firstChild;
            var staticNodes = sourceDocument.querySelectorAll('#child');
            var iterator = sourceDocument.createNodeIterator(source);
            var firstFromIterator = iterator.nextNode() === source;
            var walker = sourceDocument.createTreeWalker(source);
            var walkerFoundChild = walker.firstChild() === child;
            var observed = 0;
            source.addEventListener('adopted-test', () => observed++);
            var adopted = document.adoptNode(source);
            var identity = adopted === source && adopted.firstChild === child && child.ownerDocument === document;
            var detached = sourceDocument.querySelector('#root') === null && adopted.parentNode === null;
            adopted.dispatchEvent(new Event('adopted-test', {bubbles: true}));
            identity && detached && observed === 1 && firstFromIterator &&
              staticNodes.length === 1 && staticNodes[0] === child &&
              iterator.root === source && iterator.nextNode() === child &&
              walkerFoundChild && walker.root === source && walker.currentNode === child &&
              walker.parentNode() === source
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn document_adoption_queues_custom_element_callback_and_migrates_followup_lifecycle() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
            var lifecycle = [];
            class AdoptableElement extends HTMLElement {
              adoptedCallback(oldDocument, newDocument) {
                lifecycle.push(['adopted', oldDocument === document, newDocument === targetDocument]);
              }
              connectedCallback() { lifecycle.push(['connected', this.ownerDocument === targetDocument, this === element, this.ownerDocument === document]); }
              disconnectedCallback() { lifecycle.push(['disconnected', this.ownerDocument === targetDocument, this === element, this.ownerDocument === document]); }
            }
            customElements.define('adoptable-element', AdoptableElement);
            var targetDocument = new DOMParser().parseFromString('<main></main>', 'text/html');
            var element = document.createElement('adoptable-element');
            document.querySelector('main').appendChild(element);
            var adopted = targetDocument.adoptNode(element);
            var immediate = adopted === element && element.ownerDocument === targetDocument && element.parentNode === null;
            Promise.resolve().then(() => {
              targetDocument.body.appendChild(element);
              Promise.resolve().then(() => {
                globalThis.adoptionDone = immediate && adopted === element &&
                  lifecycle.map(entry => entry[0]).join(',') === 'connected,disconnected,adopted,connected' &&
                  lifecycle.every(entry => entry[1] === true);
              });
            });
            true
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        let result = script(&mut engine, "adoptionDone === true");
        if !matches!(result, Value::Bool(true)) {
            let diagnostic = script(
                &mut engine,
                "adoptionDone + '|owner:' + (element.ownerDocument === targetDocument) + '|detached:' + (element.parentNode === null) + '|lifecycle:' + lifecycle.map(entry => entry.join(':')).join('|')",
            );
            let diagnostic = match diagnostic {
                Value::Str(value) => value.as_str().to_owned(),
                _ => "<diagnostic was not a string>".into(),
            };
            panic!("custom-element adoption reactions were incorrect: {diagnostic}");
        }
    }

    #[test]
    fn adoption_removal_adjusts_donor_ranges_and_preserves_mutation_records() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<main><section><b>text</b></section></main>",
            96,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"
            var targetDocument = new DOMParser().parseFromString('<main></main>', 'text/html');
            var main = document.querySelector('main');
            var section = main.firstChild;
            var text = section.querySelector('b').firstChild;
            var range = document.createRange();
            range.setStart(text, 2);
            range.setEnd(text, 4);
            var observer = new MutationObserver(() => {});
            observer.observe(main, {childList: true, subtree: true});
            var adopted = targetDocument.adoptNode(section);
            var records = observer.takeRecords();
            adopted === section && records.length === 1 && records[0].removedNodes[0] === section &&
              range.startContainer === main && range.endContainer === main &&
              range.startOffset === 0 && range.endOffset === 0 &&
              range.toString() === '' && main.firstChild === null
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn adoption_rehomes_live_ranges_in_detached_subtrees() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
            var source = new DOMParser().parseFromString('<main></main>', 'text/html');
            var target = new DOMParser().parseFromString('<main></main>', 'text/html');
            var section = source.createElement('section');
            section.textContent = 'hello';
            var text = section.firstChild;
            var range = source.createRange();
            range.setStart(text, 1);
            range.setEnd(text, 4);
            var selection = source.getSelection();
            selection.setBaseAndExtent(text, 1, text, 4);
            target.adoptNode(section);
            var cloned = range.cloneRange();
            range.startContainer === text && range.toString() === 'ell' &&
              cloned.toString() === 'ell' && selection.anchorNode === text &&
              selection.toString() === 'ell' && (range.deleteContents(), text.data === 'ho')
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn adoption_clears_stale_focus_from_detached_subtree() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><input id=field></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
            var field = document.querySelector('#field');
            var blurCount = 0;
            field.addEventListener('blur', () => blurCount++);
            field.focus();
            document.querySelector('main').removeChild(field);
            var target = new DOMParser().parseFromString('<main></main>', 'text/html');
            var adopted = target.adoptNode(field);
            adopted === field && blurCount === 0 && document.activeElement === document.body
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn required_reflection_tracks_null_namespace_attributes_and_validity() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<form><input id=field><textarea id=area></textarea><select id=choice><option value='' selected>Choose</option><option value=x>Choice</option></select></form>",
            96,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"
            const field = document.getElementById('field');
            const area = document.getElementById('area');
            const choice = document.getElementById('choice');
            const initiallyAbsent = !field.required && !area.required && !choice.required;

            field.required = 'required';
            area.required = true;
            choice.required = true;
            const reflectionSetsAttributes = field.required && area.required && choice.required &&
              field.hasAttribute('required') && area.hasAttribute('required') && choice.hasAttribute('required');
            const emptyControlsAreMissing = field.validity.valueMissing && area.validity.valueMissing &&
              choice.validity.valueMissing;

            field.value = 'filled';
            area.value = 'filled';
            choice.value = 'x';
            const filledControlsAreValid = !field.validity.valueMissing && !area.validity.valueMissing &&
              !choice.validity.valueMissing;

            field.required = 0;
            area.removeAttribute('required');
            choice.required = false;
            const removalUpdatesReflection = !field.required && !area.required && !choice.required &&
              !field.hasAttribute('required') && !area.hasAttribute('required') &&
              !choice.hasAttribute('required');
            field.setAttribute('required', '');
            const attributeMutationUpdatesReflection = field.required && field.validity.valueMissing === false;
            field.removeAttribute('required');
            field.setAttributeNS('urn:test', 'test:required', '');
            const namespaceIsIgnored = !field.required && !field.validity.valueMissing;

            initiallyAbsent && reflectionSetsAttributes && emptyControlsAreMissing &&
              filledControlsAreValid && removalUpdatesReflection &&
              attributeMutationUpdatesReflection && namespaceIsIgnored
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
