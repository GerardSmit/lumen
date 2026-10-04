//! DOM objects for one Lumen realm, backed by the shared Rust render session.
use lumen::embed::{Ctx, OpError, OpResult, Value, WeakValue};
use lumen_html::{Error, Namespace, NodeId, NodeKind, html, selector, session::RenderSession};
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
};
mod error_reporting;
mod event_content_handlers;
mod events;
use events::{DomEvent, DomEventTarget, TargetData};
mod collections;
mod dataset;
mod ui_events;
use collections::{
    DescendantFilter, DomCollectionIterator, DomHtmlCollection, DomNodeList, DomTokenList,
    html_space_tokens,
};
mod style;
use style::DomStyle;
pub(crate) mod animations;
mod browser_services;
mod browsing_context;
mod realm_services;
pub use browser_services::{ClipboardHost, ClipboardOperation, ClipboardPermission};
pub use browsing_context::{
    FrameContext, FrameInstallError, FrameNavigationRequest, FrameSource, FrameUnsupportedReason,
    Origin,
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
    CanvasGpuTarget, set_gpu_canvas_context_factory, set_webgl_canvas_context_factory,
};
mod cookies;
mod cssom;
mod custom_elements;
pub use cookies::CookieHost;
mod document_utilities;
mod editing;
mod editing_history;
mod font_loading;
mod form_data_bridge;
pub mod forms;
mod geometry;
mod image_loading;
mod media;
mod media_capture;
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
pub use script_loading::{
    DeclaredScriptType as ScriptType, ScriptDescriptor, UnhandledScriptActivation,
    UnhandledScriptReason,
};
mod jsx;
pub mod layout_observers;
mod observers;
mod range;
mod reactive;
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
        | lumen_html::observe::ObservedKind::CharacterData { .. } => false,
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

pub struct DomRealm {
    media_capture: media_capture::RealmMediaCapture,
    browser_services: browser_services::RealmBrowserServices,
    document_url: RefCell<Option<String>>,
    about_base_url: RefCell<Option<String>>,
    document_base_url: RefCell<DocumentBaseUrlCache>,
    cookie_host: RefCell<Option<Rc<dyn CookieHost>>>,
    content_type: String,
    is_html_document: bool,
    has_browsing_context: bool,
    file_picker_host: RefCell<Option<Rc<dyn Fn(forms::FilePickerRequest) -> Result<(), String>>>>,
    form_submission_host:
        RefCell<Option<Rc<dyn Fn(forms::FormSubmissionRequest) -> Result<(), String>>>>,
    canvases: canvas::CanvasRegistry,
    forms: RefCell<forms::FormState>,
    ranges: Rc<range::RangeRegistry>,
    iterators: Rc<document_utilities::IteratorRegistry>,
    tree_walkers: Rc<document_utilities::TreeWalkerRegistry>,
    selection: RefCell<Option<Rc<range::SelectionData>>>,
    selection_wrapper: RefCell<Option<WeakValue>>,
    ready_state: Cell<DocumentReadyState>,
    current_script: Cell<Option<NodeId>>,
    scripts: RefCell<script_loading::ScriptLoader>,
    module_activations_enabled: Cell<bool>,
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
    browsing_context: RefCell<std::rc::Weak<browsing_context::BrowsingContext>>,
    frame_contexts: RefCell<HashMap<NodeId, std::rc::Weak<browsing_context::BrowsingContext>>>,
    focused: Cell<Option<NodeId>>,
    selections: RefCell<HashMap<NodeId, (usize, usize, String)>>,
    editing: RefCell<EditingState>,
    programmatic_value_epoch: Cell<u64>,
    programmatic_value_writes: RefCell<ValueWriteJournal>,
}

impl DomRealm {
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

    fn flush_script_activations(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if !self.has_browsing_context {
            return Ok(());
        }
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
        let first_assignment = previous_url.is_none();
        *self.document_url.borrow_mut() = Some(url.clone());

        // During install the host supplies the response URL just after the
        // native document is created. If a getter ran before then, discard
        // that provisional about:blank calculation and freeze the initial
        // parsed base against the actual response fallback.
        if first_assignment {
            *self.document_base_url.borrow_mut() = DocumentBaseUrlCache::default();
        }
        if let Some(context) = self.browsing_context() {
            browsing_context::update_document_origin(&context, &url);
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

        *self.about_base_url.borrow_mut() = url;
    }

    pub fn document_url(&self) -> Option<String> {
        self.document_url.borrow().clone()
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
    ) {
        if !script_loading::is_connected(document, mutation.target) {
            return;
        }
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
            | lumen_html::observe::ObservedKind::CharacterData { .. } => (false, false),
        };
        if relevant {
            self.recompute_document_base_url(document, refreeze_selected_base);
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
        *self.form_submission_host.borrow_mut() = Some(callback);
    }

    pub fn submit_form(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        form: NodeId,
        submitter: Option<NodeId>,
    ) -> OpResult<bool> {
        let request = forms::request_submission(ctx, self, form, submitter, &self.forms)?;
        let Some(request) = request else {
            return Ok(false);
        };
        let callback = self.form_submission_host.borrow().clone().ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "the embedder has not supplied form navigation",
            )
        })?;
        callback(request).map_err(|message| OpError::new("NotSupportedError", message))?;
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
        self.media.borrow().snapshot(node)
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
                if realm.media_snapshot(node).generation == generation
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
        self.flush_layout()?;
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
        let pending_nodes = self.images.pending_nodes();
        let pending_nodes = pending_nodes
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        let completion_nodes = completions
            .iter()
            .map(|completion| completion.node)
            .collect::<std::collections::HashSet<_>>();

        let mut event_roots = Vec::with_capacity(completions.len());
        {
            let mut roots = self.image_request_roots.borrow_mut();
            let obsolete = roots
                .keys()
                .copied()
                .filter(|node| !pending_nodes.contains(node) && !completion_nodes.contains(node))
                .collect::<Vec<_>>();
            for node in obsolete {
                roots.remove(&node);
            }
            for &node in &pending_nodes {
                if !roots.contains_key(&node) {
                    roots.insert(node, self.retain_image_request(ctx, node));
                }
            }
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

    pub fn focus(self: &Rc<Self>, ctx: &mut Ctx, node: Option<NodeId>) -> OpResult<()> {
        if let Some(node) = node {
            if !self.focus_rendered(node)? {
                return Ok(());
            }
            let session = self.session.borrow();
            let document = session.document();
            let NodeKind::Element {
                name, attributes, ..
            } = document.kind(node).map_err(dom_error)?
            else {
                return Err(OpError::new("TypeError", "focus target must be an element"));
            };
            let focusable = matches!(
                name.as_str(),
                "input" | "button" | "textarea" | "select" | "summary"
            ) || attributes.iter().any(|(attribute, _)| {
                attribute == "tabindex"
                    || (attribute == "contenteditable"
                        && attributes
                            .iter()
                            .any(|(name, value)| name == "contenteditable" && value != "false"))
                    || (attribute == "href" && matches!(name.as_str(), "a" | "area"))
            });
            if !focusable {
                return Ok(());
            }
            let mut ancestor = node;
            while ancestor != document.root() {
                let Some(parent) = document
                    .shadow_including_parent(ancestor)
                    .map_err(dom_error)?
                else {
                    return Ok(());
                };
                ancestor = parent;
            }
        }
        let old = self.focused_node();
        if old == node {
            return Ok(());
        }
        self.focused.set(None);
        let previous = old.map_or(Value::Null, |old| self.wrap(ctx, old));
        let next_value = node.map_or(Value::Null, |node| self.wrap(ctx, node));
        if let Some(old) = old {
            self.dispatch(
                ctx,
                old,
                "blur",
                false,
                false,
                &[("relatedTarget", next_value.clone())],
            )?;
            self.dispatch(
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
        if let Some(node) = node {
            if self.focused_node() != Some(node) {
                self.focused.set(None);
                return Ok(());
            }
            self.dispatch(
                ctx,
                node,
                "focus",
                false,
                false,
                &[("relatedTarget", previous.clone())],
            )?;
            if self.focused_node() == Some(node) {
                self.dispatch(
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
        let data = self
            .targets
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
            .ok_or_else(|| {
                lumen::embed::OpError::new("InvalidStateError", "event target no longer exists")
            })?;
        let event = lumen::embed::JsObject::from_value(event).expect("event object");
        let allowed = if trusted {
            events::dispatch_user_agent_event(ctx, lumen_bind::This(value), event)
        } else {
            DomEventTarget::dispatch_event(ctx, lumen_bind::This(value), event)
        }?;
        if allowed && kind == "keydown" && self.focused_node() == Some(node) {
            self.edit_control_key(ctx, node, properties)?;
        }
        Ok(allowed)
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
        work(&mut self.session.borrow_mut())
    }

    pub fn wrapper_count(&self) -> usize {
        self.wrappers.borrow().len()
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
        self.ranges.adopt_nodes(&target.ranges, target, mapping);
        if let Some(selection) = self.selection.borrow().as_ref() {
            selection.adopt_nodes(mapping);
        }
        animations::adopt_nodes(ctx, self, target, mapping)?;
        self.canvases
            .adopt_nodes_into(ctx, &target.canvases, target, mapping)?;
        self.images.adopt_nodes_into(&target.images, mapping);
        self.iterators
            .adopt_nodes(self, target, &target.iterators, mapping);
        self.tree_walkers
            .adopt_nodes(self, target, &target.tree_walkers, mapping);
        observers::adopt_nodes(ctx, self, target, mapping);
        Ok(())
    }

    fn adopt_node_from(
        target: &Rc<Self>,
        ctx: &mut Ctx,
        source: &Rc<Self>,
        source_id: NodeId,
    ) -> OpResult<NodeId> {
        {
            let session = source.session.borrow();
            let document = session.document();
            if matches!(document.kind(source_id).map_err(dom_error)?, NodeKind::Document)
                || document.shadow_host(source_id).map_err(dom_error)?.is_some()
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
            }) if name == "html" => ctx.cached_instance(id, || DomHtmlHtmlElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "head" => ctx.cached_instance(id, || DomHtmlHeadElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "body" => ctx.cached_instance(id, || DomHtmlBodyElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "title" => ctx.cached_instance(id, || DomHtmlTitleElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
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
            }) if name == "link" => ctx.cached_instance(id, || DomHtmlLinkElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
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
            }) if name == "img" => ctx.cached_instance(id, || DomHtmlImageElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "audio" => {
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
            }) if name == "video" => {
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
            }) if name == "canvas" => ctx.cached_instance(id, || {
                canvas::DomCanvasElement::from_node(DomHtmlElement {
                    base: DomElement { base: node },
                })
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "form" => ctx.cached_instance(id, || DomFormElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "style" => ctx.cached_instance(id, || DomStyleElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "template" => ctx.cached_instance(id, || DomTemplateElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "iframe" => ctx.cached_instance(id, || DomIFrameElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "input" => ctx.cached_instance(id, || DomInputElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "select" => ctx.cached_instance(id, || DomSelectElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "option" => ctx.cached_instance(id, || DomOptionElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "textarea" => ctx.cached_instance(id, || DomTextAreaElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            }) if name == "slot" => ctx.cached_instance(id, || DomSlotElement {
                base: DomHtmlElement {
                    base: DomElement { base: node },
                },
            }),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                ..
            }) => ctx.cached_instance(id, || DomHtmlElement {
                base: DomElement { base: node },
            }),
            Ok(NodeKind::Element { .. }) => ctx.cached_instance(id, || DomElement { base: node }),
            Ok(NodeKind::Text(_)) => ctx.cached_instance(id, || DomText {
                base: DomCharacterData { base: node },
            }),
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
        Error::Hierarchy => "HierarchyRequestError",
        Error::LimitExceeded => "QuotaExceededError",
        Error::IndexSize => "IndexSizeError",
        Error::WrongKind | Error::UnsupportedDoctype => "NotSupportedError",
    };
    OpError::new(name, format!("DOM operation failed: {error:?}"))
}

fn selector_error(error: selector::SelectorError) -> OpError {
    OpError::new("SyntaxError", format!("Invalid selector: {error:?}"))
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

fn namespace_for_qname(namespace_uri: Option<&str>, qualified_name: &str) -> OpResult<Namespace> {
    let Some((prefix, _local_name)) = lumen_html::xml::split_qname(qualified_name) else {
        return Err(OpError::new(
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
        return Err(OpError::new(
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
        let mut document = lumen_html::Document::new(100_000);
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
            Some(name) => Some(namespace_for_qname(namespace_uri, name)?),
        };
        let content_type = match namespace_uri {
            Some("http://www.w3.org/1999/xhtml") => "application/xhtml+xml",
            Some("http://www.w3.org/2000/svg") => "image/svg+xml",
            _ => "application/xml",
        };
        let document = lumen_html::Document::new(100_000);
        let realm =
            DomRealm::realm_from_document_with_metadata(document, content_type, false, false);
        realm.set_document_url("about:blank");

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
    let mut cursor = next_descendant(document, root, root)?;
    while let Some(id) = cursor {
        if let NodeKind::Element { attributes, .. } = document.kind(id)? {
            if attributes
                .iter()
                .any(|(name, value)| name == "id" && value == wanted)
            {
                return Ok(Some(id));
            }
        }
        cursor = next_descendant(document, root, id)?;
    }
    Ok(None)
}

fn next_descendant(
    document: &lumen_html::Document,
    root: NodeId,
    mut node: NodeId,
) -> Result<Option<NodeId>, Error> {
    if let Some(child) = document.first_child(node)? {
        return Ok(Some(child));
    }
    loop {
        if node == root {
            return Ok(None);
        }
        if let Some(sibling) = document.next_sibling(node)? {
            return Ok(Some(sibling));
        }
        node = document.parent(node)?.ok_or(Error::Hierarchy)?;
    }
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

#[lumen_bind::class(name = "Element", extends = DomNode, hint(js(webidl)))]
pub struct DomElement {
    base: DomNode,
}
#[lumen_bind::methods]
impl DomElement {
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

    fn get_animations(&self, ctx: &mut Ctx) -> OpResult<Value> {
        animations::for_element(ctx, &self.base.realm, self.base.id)
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
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.scroll_left)),
        )
    }

    #[getter]
    fn scroll_top(&self) -> OpResult<f64> {
        self.base.realm.flush_layout()?;
        Ok(
            geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
                .map_or(0.0, |geometry| f64::from(geometry.scroll_top)),
        )
    }

    #[getter]
    fn offset_parent(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.base.realm.flush_layout()?;
        let parent = geometry::snapshot(&mut self.base.realm.session.borrow_mut(), self.base.id)
            .and_then(|geometry| geometry.offset_parent);
        Ok(self.base.realm.wrap_option(ctx, parent))
    }

    #[setter]
    fn set_scroll_left(&self, value: f64) -> OpResult<()> {
        self.base.realm.flush_layout()?;
        let mut session = self.base.realm.session.borrow_mut();
        let (_, top) = session.scroll_offset(self.base.id);
        session
            .set_scroll_offset(
                self.base.id,
                if value.is_finite() { value as f32 } else { 0.0 },
                top,
            )
            .map_err(|error| {
                OpError::new("InvalidStateError", format!("scroll failed: {error:?}"))
            })?;
        Ok(())
    }

    #[setter]
    fn set_scroll_top(&self, value: f64) -> OpResult<()> {
        self.base.realm.flush_layout()?;
        let mut session = self.base.realm.session.borrow_mut();
        let (left, _) = session.scroll_offset(self.base.id);
        session
            .set_scroll_offset(
                self.base.id,
                left,
                if value.is_finite() { value as f32 } else { 0.0 },
            )
            .map_err(|error| {
                OpError::new("InvalidStateError", format!("scroll failed: {error:?}"))
            })?;
        Ok(())
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
        let root = self
            .base
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attach_shadow(self.base.id, mode)
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
        Ok(self.base.get_attribute("slot")?.unwrap_or_default())
    }
    #[setter(coerce)]
    fn set_slot(&self, value: &str) -> OpResult<()> {
        self.base.set_attribute_core("slot", value)
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
        let Some(href) = href else {
            return Ok(String::new());
        };
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
            .get_attribute("target")?
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
        let Some(href) = node.get_attribute("href")? else {
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
    #[getter(name = "async")]
    fn script_async(&self) -> OpResult<bool> {
        let node = &self.base.base.base;
        Ok(node.realm.scripts.borrow().force_async(node.id) || node.has_attribute("async")?)
    }

    #[setter(name = "async")]
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
        self.base.base.base.has_attribute("defer")
    }

    #[setter(name = "defer")]
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
        self.base.base.base.has_attribute("nomodule")
    }

    #[setter(name = "noModule")]
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
            .get_attribute("type")?
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
        let Some(source) = node.get_attribute("src")? else {
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
        let session = node.realm.session.borrow();
        node.realm
            .images
            .snapshot(session.document(), node.id, &base)
    }
}

#[lumen_bind::methods]
impl DomHtmlImageElement {
    /// The legacy `Image(width, height)` factory uses the same native class as
    /// queried and `createElement("img")` elements, preserving node identity.
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[default(0)] width: u32,
        #[default(0)] height: u32,
    ) -> OpResult<Self> {
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
        if let Some(weak) = ctx.weak_value(&this.0) {
            realm.wrappers.borrow_mut().insert(id, weak);
        }
        Ok(Self {
            base: DomHtmlElement {
                base: DomElement { base: node },
            },
        })
    }

    #[getter]
    fn src(&self) -> OpResult<String> {
        let node = self.image_node();
        let Some(source) = node.get_attribute("src")? else {
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
        let value = self.image_node().get_attribute("crossorigin")?;
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
        match node.get_attribute("width")? {
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
        match node.get_attribute("height")? {
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

#[lumen_bind::class(name = "HTMLFormElement", extends = DomHtmlElement, hint(js(webidl)))]
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

    fn request_submit(&self, ctx: &mut Ctx, submitter: Option<&DomNode>) -> OpResult<()> {
        let node = &self.base.base.base;
        let submitter = if let Some(submitter) = submitter {
            if !Rc::ptr_eq(&node.realm, &submitter.realm) {
                return Err(OpError::new(
                    "NotFoundError",
                    "Submitter belongs to another document",
                ));
            }
            let kind = submitter.local_name()?.unwrap_or_default();
            let input_type = submitter.get_attribute("type")?.unwrap_or_else(|| {
                if kind == "button" {
                    "submit".into()
                } else {
                    "text".into()
                }
            });
            if !(kind == "button" && input_type.eq_ignore_ascii_case("submit")
                || kind == "input"
                    && (input_type.eq_ignore_ascii_case("submit")
                        || input_type.eq_ignore_ascii_case("image")))
            {
                return Err(OpError::new(
                    "TypeError",
                    "Submitter must be a submit button",
                ));
            }
            if !lumen_html::forms::form_controls(node.realm.session.borrow().document(), node.id)
                .contains(&submitter.id)
            {
                return Err(OpError::new(
                    "NotFoundError",
                    "Submitter does not belong to this form",
                ));
            }
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
        let controls =
            lumen_html::forms::form_controls(node.realm.session.borrow().document(), node.id);
        let mut valid = true;
        for control in controls {
            valid &= forms::check_validity(ctx, &node.realm, control, &node.realm.forms)?;
        }
        Ok(valid)
    }
}
#[lumen_bind::class(name = "HTMLInputElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomInputElement {
    base: DomHtmlElement,
}

#[lumen_bind::class(name = "HTMLSelectElement", extends = DomHtmlElement, hint(js(webidl)))]
pub struct DomSelectElement {
    base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomSelectElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "select")
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
    #[setter]
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
        self.base.base.base.has_attribute("selected")
    }
    #[setter]
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
#[lumen_bind::methods]
impl DomTextAreaElement {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
    ) -> OpResult<custom_elements::HtmlElementCtor> {
        custom_elements::construct_customized_element(ctx, this.0, "textarea")
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
            .get_attribute("type")?
            .unwrap_or_else(|| "text".into()))
    }
    #[setter(name = "type", coerce)]
    fn set_input_type(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("type", value)
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        self.base.value()
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        self.base.set_value(value)
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
    fn validity(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        forms::validity_object(ctx, &node.realm, node.id, &node.realm.forms)
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
        if let Some(value) = self.base.base.get_attribute("tabindex")? {
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
                && self.base.base.has_attribute("href")?)
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
    fn focus(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.base.base.realm.focus(ctx, Some(self.base.base.id))
    }
    fn blur(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.base.base.realm.focused_node() == Some(self.base.base.id) {
            self.base.base.realm.focus(ctx, None)?;
        }
        Ok(())
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        if let Some(value) = self.base.base.get_attribute("value")? {
            return Ok(value);
        }
        if self.base.base.local_name()?.as_deref() == Some("textarea") {
            return Ok(self.base.base.text_content()?.unwrap_or_default());
        }
        Ok(String::new())
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        forms::set_control_value(
            &self.base.base.realm,
            &mut self.base.base.realm.forms.borrow_mut(),
            self.base.base.id,
            value,
        )?;
        let end = value.encode_utf16().count();
        self.base
            .base
            .realm
            .set_selection(self.base.base.id, end, end, "none")
    }
    #[getter]
    fn selection_start(&self) -> OpResult<usize> {
        Ok(self.base.base.realm.selection(self.base.base.id)?.0)
    }
    #[setter]
    fn set_selection_start(&self, start: usize) -> OpResult<()> {
        let (_, end, direction) = self.base.base.realm.selection(self.base.base.id)?;
        self.base
            .base
            .realm
            .set_selection(self.base.base.id, start, end.max(start), &direction)
    }
    #[getter]
    fn selection_end(&self) -> OpResult<usize> {
        Ok(self.base.base.realm.selection(self.base.base.id)?.1)
    }
    #[setter]
    fn set_selection_end(&self, end: usize) -> OpResult<()> {
        let (start, _, direction) = self.base.base.realm.selection(self.base.base.id)?;
        self.base
            .base
            .realm
            .set_selection(self.base.base.id, start, end, &direction)
    }
    #[getter]
    fn selection_direction(&self) -> OpResult<String> {
        Ok(self.base.base.realm.selection(self.base.base.id)?.2)
    }
    #[setter(coerce)]
    fn set_selection_direction(&self, direction: &str) -> OpResult<()> {
        let (start, end, _) = self.base.base.realm.selection(self.base.base.id)?;
        self.base
            .base
            .realm
            .set_selection(self.base.base.id, start, end, direction)
    }
    #[method(coerce)]
    fn set_selection_range(
        &self,
        start: usize,
        end: usize,
        direction: Option<String>,
    ) -> OpResult<()> {
        self.base.base.realm.set_selection(
            self.base.base.id,
            start,
            end,
            direction.as_deref().unwrap_or("none"),
        )
    }
    fn select(&self) -> OpResult<()> {
        let end = self
            .base
            .base
            .realm
            .control_value(self.base.base.id)?
            .encode_utf16()
            .count();
        self.base
            .base
            .realm
            .set_selection(self.base.base.id, 0, end, "none")
    }
    #[getter]
    fn checked(&self) -> OpResult<bool> {
        self.base.base.has_attribute("checked")
    }
    #[setter]
    fn set_checked(&self, checked: bool) -> OpResult<()> {
        forms::capture_defaults(
            &mut self.base.base.realm.forms.borrow_mut(),
            self.base.base.realm.session.borrow().document(),
            self.base.base.id,
        );
        if checked {
            self.base.base.set_attribute_core("checked", "")
        } else {
            self.base.base.remove_attribute_core("checked")
        }
    }
    #[getter]
    fn disabled(&self) -> OpResult<bool> {
        self.base.base.has_attribute("disabled")
    }
    #[setter]
    fn set_disabled(&self, disabled: bool) -> OpResult<()> {
        if disabled {
            self.base.base.set_attribute_core("disabled", "")
        } else {
            self.base.base.remove_attribute_core("disabled")
        }
    }
    #[getter]
    fn read_only(&self) -> OpResult<bool> {
        self.base.base.has_attribute("readonly")
    }
    #[setter]
    fn set_read_only(&self, read_only: bool) -> OpResult<()> {
        if read_only {
            self.base.base.set_attribute_core("readonly", "")
        } else {
            self.base.base.remove_attribute_core("readonly")
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
    fn set_data(&self, value: &str) -> OpResult<()> {
        self.base.set_node_value(Some(value))
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
    fn substring_data(&self, offset: usize, count: usize) -> OpResult<String> {
        self.base
            .realm
            .session
            .borrow()
            .document()
            .substring_data(self.base.id, offset, count)
            .map_err(dom_error)
    }

    #[method(name = "appendData", coerce)]
    fn append_data(&self, data: &str) -> OpResult<()> {
        self.mutate_data(|document| document.append_data(self.base.id, data))
    }

    #[method(name = "insertData", coerce)]
    fn insert_data(&self, offset: usize, data: &str) -> OpResult<()> {
        self.mutate_data(|document| document.insert_data(self.base.id, offset, data))
    }

    #[method(name = "deleteData", coerce)]
    fn delete_data(&self, offset: usize, count: usize) -> OpResult<()> {
        self.mutate_data(|document| document.delete_data(self.base.id, offset, count))
    }

    #[method(name = "replaceData", coerce)]
    fn replace_data(&self, offset: usize, count: usize, data: &str) -> OpResult<()> {
        self.mutate_data(|document| document.replace_data_range(self.base.id, offset, count, data))
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
impl DomText {}

#[lumen_bind::class(name = "Comment", extends = DomCharacterData, hint(js(webidl)))]
pub struct DomComment {
    base: DomCharacterData,
}
#[lumen_bind::methods]
impl DomComment {}

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
#[lumen_bind::methods]
impl DomDocumentFragment {}

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
    #[getter]
    fn cookie(&self) -> OpResult<String> {
        self.cookie_value()
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
        "UTF-8"
    }

    #[getter]
    fn charset(&self) -> &'static str {
        "UTF-8"
    }

    #[getter(name = "inputEncoding")]
    fn input_encoding(&self) -> &'static str {
        "UTF-8"
    }

    #[getter]
    fn location(&self) -> Value {
        if self.realm.has_browsing_context {
            Value::Undefined
        } else {
            Value::Null
        }
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
    #[getter]
    fn document_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let id = children(document, document.root())
            .map_err(dom_error)?
            .into_iter()
            .find(|id| matches!(document.kind(*id), Ok(NodeKind::Element { .. })));
        drop(session);
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
        let namespace = namespace_for_qname(namespace_uri, name)?;
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
        if !lumen_html::xml::is_xml_name(target) || target.eq_ignore_ascii_case("xml") {
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
            .map_err(selector_error)?;
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

impl DomNode {
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
        self.realm.invalidate_textarea_ancestor(parent);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
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
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        Ok(())
    }
    fn replace_children(&self, ctx: &mut Ctx, #[varargs] nodes: Vec<Value>) -> OpResult<()> {
        let (ids, generated) = converted_dom_nodes(ctx, &self.realm, nodes)?;
        let mut session = self.realm.session.borrow_mut();
        let old = children(session.document(), self.id).map_err(dom_error)?;
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

    #[method(coerce)]
    fn query_selector_all(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        query: &str,
    ) -> OpResult<DomNodeList> {
        let nodes =
            selector::query_selector_all(self.realm.session.borrow().document(), self.id, query)
                .map_err(selector_error)?;
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
            NodeKind::ProcessingInstruction { data, .. } => Some(data.clone()),
            _ => None,
        })
    }

    #[setter(coerce)]
    fn set_node_value(&self, value: Option<&str>) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let is_text = matches!(doc.kind(self.id).map_err(dom_error)?, NodeKind::Text(_));
        let result = match doc.kind(self.id).map_err(dom_error)? {
            NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {
                doc.replace_data(self.id, value.unwrap_or(""))
                    .map_err(dom_error)
            }
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
            NodeKind::Text(_) => 3,
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
            NodeKind::Text(_) => "#text".into(),
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

    fn append_child(&self, ctx: &mut Ctx, child: &DomNode) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        if !Rc::ptr_eq(&self.realm, &child.realm) {
            return Err(OpError::new(
                "InvalidStateError",
                "nodes belong to different documents",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .append(self.id, child.id)
            .map_err(dom_error)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        Ok(self.realm.wrap(ctx, child.id))
    }

    fn replace_child(
        &self,
        ctx: &mut Ctx,
        new_child: &DomNode,
        old_child: &DomNode,
    ) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        if !Rc::ptr_eq(&self.realm, &new_child.realm) || !Rc::ptr_eq(&self.realm, &old_child.realm)
        {
            return Err(OpError::new(
                "WrongDocumentError",
                "nodes belong to different documents",
            ));
        }
        if self
            .realm
            .session
            .borrow()
            .document()
            .parent(old_child.id)
            .map_err(dom_error)?
            != Some(self.id)
        {
            return Err(OpError::new("NotFoundError", "node is not a child"));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .replace(old_child.id, new_child.id)
            .map_err(dom_error)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        let removed = self.realm.wrap(ctx, old_child.id);
        self.realm.reap_detached([old_child.id]);
        Ok(removed)
    }

    fn insert_before(
        &self,
        ctx: &mut Ctx,
        child: &DomNode,
        before: Option<&DomNode>,
    ) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        if !Rc::ptr_eq(&self.realm, &child.realm)
            || before.is_some_and(|node| !Rc::ptr_eq(&self.realm, &node.realm))
        {
            return Err(OpError::new(
                "InvalidStateError",
                "nodes belong to different documents",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .insert_before(self.id, child.id, before.map(|node| node.id))
            .map_err(dom_error)?;
        self.realm.invalidate_textarea_ancestor(self.id);
        self.realm.flush_script_activations(ctx)?;
        Ok(self.realm.wrap(ctx, child.id))
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
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(child.id)
            .map_err(dom_error)?;
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
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(self.id)
            .map_err(dom_error)?;
        if let Some(parent) = parent {
            self.realm.invalidate_textarea_ancestor(parent);
        }
        self.realm.reap_detached([self.id]);
        Ok(())
    }

    fn clone_node(&self, ctx: &mut Ctx, deep: Option<bool>) -> OpResult<Value> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let id = doc
            .clone_node(self.id, deep.unwrap_or(false))
            .map_err(dom_error)?;
        let script_pairs = script_loading::paired_subtree_nodes(
            session.document(),
            session.document(),
            self.id,
            id,
        );
        drop(session);
        self.realm.scripts.borrow_mut().clone_states(&script_pairs);
        event_content_handlers::initialize_subtree(ctx, &self.realm, id)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(coerce)]
    fn set_attribute(&self, ctx: &mut Ctx, name: &str, value: &str) -> OpResult<()> {
        self.set_attribute_core(name, value)?;
        event_content_handlers::attribute_changed(
            ctx,
            &self.realm,
            self.id,
            None,
            name,
            Some(value),
        )?;
        self.realm.flush_script_activations(ctx)
    }

    fn set_attribute_core(&self, name: &str, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(self.id, name, value)
            .map_err(dom_error)?;
        if matches!(name, "width" | "height") {
            self.realm.sync_canvas()?;
        }
        if name.eq_ignore_ascii_case("value") {
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
        let session = self.realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(self.id).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "attributes require an element"));
        };
        Ok(attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone()))
    }

    #[method(name = "getAttributeNS", coerce)]
    fn get_attribute_ns(
        &self,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<Option<String>> {
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
        let invalid_name = qualified_name.is_empty()
            || qualified_name.starts_with(':')
            || qualified_name.ends_with(':')
            || qualified_name.matches(':').count() > 1;
        if invalid_name {
            return Err(OpError::new(
                "InvalidCharacterError",
                "attribute name is not a valid qualified name",
            ));
        }
        let (prefix, _) = qualified_name
            .split_once(':')
            .map_or(("", qualified_name), |(prefix, local)| (prefix, local));
        let xml_uri = "http://www.w3.org/XML/1998/namespace";
        let xmlns_uri = "http://www.w3.org/2000/xmlns/";
        if (!prefix.is_empty() && namespace_uri.is_none())
            || (prefix == "xml" && namespace_uri != Some(xml_uri))
            || ((qualified_name == "xmlns" || prefix == "xmlns")
                != (namespace_uri == Some(xmlns_uri)))
        {
            return Err(OpError::new(
                "NamespaceError",
                "attribute prefix and namespace URI do not match",
            ));
        }
        let _html_allocations = enter_html_allocation_category();
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.id, namespace_uri, qualified_name, value)
            .map_err(dom_error)?;
        if namespace_uri.is_none() && matches!(qualified_name, "width" | "height") {
            self.realm.sync_canvas()?;
        }
        self.realm.sync_image_bitmaps()?;
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
        let _html_allocations = enter_html_allocation_category();
        let existed = self
            .realm
            .session
            .borrow()
            .document()
            .get_attribute_ns(self.id, namespace_uri, local_name)
            .map_err(dom_error)?
            .is_some();
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute_ns(self.id, namespace_uri, local_name)
            .map_err(dom_error)?;
        self.realm.sync_image_bitmaps()?;
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
        Ok(self.get_attribute("id")?.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_id(&self, value: &str) -> OpResult<()> {
        self.set_attribute_core("id", value)
    }

    #[getter]
    fn class_name(&self) -> OpResult<String> {
        Ok(self.get_attribute("class")?.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_class_name(&self, value: &str) -> OpResult<()> {
        self.set_attribute_core("class", value)
    }

    #[method(coerce)]
    fn remove_attribute(&self, ctx: &mut Ctx, name: &str) -> OpResult<()> {
        let existed = self.get_attribute(name)?.is_some();
        let namespace_uri = self
            .realm
            .session
            .borrow()
            .document()
            .attribute_namespace_uri(self.id, name)
            .map_err(dom_error)?;
        self.remove_attribute_core(name)?;
        if existed {
            event_content_handlers::attribute_changed(
                ctx,
                &self.realm,
                self.id,
                namespace_uri.as_deref(),
                name,
                None,
            )?;
        }
        self.realm.flush_script_activations(ctx)
    }

    fn remove_attribute_core(&self, name: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(self.id, name)
            .map_err(dom_error)?;
        if name.eq_ignore_ascii_case("value") {
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
            NodeKind::Text(text) | NodeKind::Comment(text) => return Ok(Some(text.clone())),
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
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let old = if matches!(
            doc.kind(self.id).map_err(dom_error)?,
            NodeKind::Element { .. } | NodeKind::DocumentFragment
        ) {
            children(doc, self.id).map_err(dom_error)?
        } else {
            Vec::new()
        };
        let result = match doc.kind(self.id).map_err(dom_error)? {
            NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {
                doc.replace_data(self.id, value).map_err(dom_error)
            }
            NodeKind::Element { .. } | NodeKind::DocumentFragment => {
                let replacement = if value.is_empty() {
                    doc.create(NodeKind::DocumentFragment)
                } else {
                    doc.create(NodeKind::Text(value.to_owned()))
                }
                .map_err(dom_error)?;
                let result = doc
                    .replace_children(self.id, replacement)
                    .map_err(dom_error);
                if value.is_empty() {
                    doc.destroy_subtree(replacement).map_err(dom_error)?;
                }
                result
            }
            _ => Ok(()),
        };
        drop(session);
        if result.is_ok() {
            self.realm.invalidate_textarea_ancestor(self.id);
            self.realm.reap_detached(old);
        }
        result?;
        self.realm.flush_script_activations(ctx)
    }

    #[getter(name = "innerHTML")]
    fn inner_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        html::inner_html(session.document(), self.id).map_err(dom_error)
    }

    #[setter(name = "innerHTML", coerce)]
    fn set_inner_html(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let _html_allocations = enter_html_allocation_category();
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let context = document
            .shadow_host(self.id)
            .map_err(dom_error)?
            .unwrap_or(self.id);
        let fragment = html::parse_fragment_in(document, context, value).map_err(|error| {
            OpError::new(
                "InvalidStateError",
                format!("HTML parse error at {}: {}", error.offset, error.message),
            )
        })?;
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
        let result = document
            .replace_children(target, fragment)
            .map_err(dom_error);
        document.destroy_subtree(fragment).map_err(dom_error)?;
        drop(session);
        if result.is_ok() {
            self.realm.invalidate_textarea_ancestor(self.id);
            self.realm.reap_detached(old);
            for node in inserted {
                event_content_handlers::initialize_subtree(ctx, &self.realm, node)?;
            }
        }
        result?;
        self.realm.flush_script_activations(ctx)
    }

    #[getter(name = "outerHTML")]
    fn outer_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        html::outer_html(session.document(), self.id).map_err(dom_error)
    }

    #[method(coerce)]
    fn query_selector(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id =
            selector::query_selector(session.document(), self.id, query).map_err(selector_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }

    #[method(coerce)]
    fn matches(&self, query: &str) -> OpResult<bool> {
        let session = self.realm.session.borrow();
        selector::matches(session.document(), self.id, query).map_err(selector_error)
    }

    #[method(coerce)]
    fn closest(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::closest(session.document(), self.id, query).map_err(selector_error)?;
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
            media_capture: media_capture::RealmMediaCapture::default(),
            browser_services: browser_services::RealmBrowserServices::default(),
            document_url: RefCell::new(None),
            about_base_url: RefCell::new(None),
            document_base_url: RefCell::new(DocumentBaseUrlCache::default()),
            content_type: content_type.to_owned(),
            is_html_document,
            has_browsing_context,
            file_picker_host: RefCell::new(None),
            form_submission_host: RefCell::new(None),
            canvases: canvas::CanvasRegistry::default(),
            forms: RefCell::new(forms::FormState::default()),
            ranges: range::RangeRegistry::new(),
            iterators: document_utilities::IteratorRegistry::new(),
            tree_walkers: document_utilities::TreeWalkerRegistry::new(),
            selection: RefCell::new(None),
            selection_wrapper: RefCell::new(None),
            ready_state: Cell::new(DocumentReadyState::Loading),
            current_script: Cell::new(None),
            scripts: RefCell::new(script_loading::ScriptLoader::default()),
            module_activations_enabled: Cell::new(false),
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
            browsing_context: RefCell::new(std::rc::Weak::new()),
            frame_contexts: RefCell::new(HashMap::new()),
            focused: Cell::new(None),
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
                    realm.observe_base_url_mutation(document, mutation);
                    realm.canvases.on_mutation(document, mutation);
                    realm.images.on_mutation(document, mutation);
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
                    let sinks = realm.mutation_sinks.borrow().clone();
                    for sink in sinks {
                        sink(document, mutation);
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
        let document = if self.is_html_document {
            ctx.new_instance(document)
        } else {
            ctx.new_instance(DomXmlDocument { base: document })
        };
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
    let document = html::parse_with_declarative_shadow_roots(source, max_nodes, true)
        .map_err(InstallError::Parse)?;
    let context = browsing_context::root_context(ctx).map_err(|_| InstallError::Global)?;
    install_document(
        ctx,
        document,
        "text/html",
        true,
        true,
        Some(context),
        None,
        None,
    )
}

/// Parse and install an active XHTML document.
///
/// Unlike [`install`], this uses the bounded XML parser: markup is required to
/// be well-formed, names retain their case, and external entities are never
/// fetched. The resulting XMLDocument still has a live browsing context.
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
    let document = lumen_html::xml::parse(source, max_nodes).map_err(InstallError::XmlParse)?;
    let context = browsing_context::root_context(ctx).map_err(|_| InstallError::Global)?;
    install_document(
        ctx,
        document,
        document_type.content_type(),
        false,
        true,
        Some(context),
        None,
        None,
    )
}

fn install_document(
    ctx: &mut Ctx,
    mut document: lumen_html::Document,
    content_type: &str,
    is_html_document: bool,
    has_browsing_context: bool,
    context: Option<Rc<browsing_context::BrowsingContext>>,
    document_url: Option<String>,
    about_base_url: Option<String>,
) -> Result<Rc<DomRealm>, InstallError> {
    document.set_html_document(is_html_document);
    let realm = DomRealm::realm_from_document_with_metadata(
        document,
        content_type,
        is_html_document,
        has_browsing_context,
    );
    let window_proxy = if let Some(context) = context.as_ref() {
        browsing_context::register_context_service(ctx, context.clone());
        *realm.browsing_context.borrow_mut() = Rc::downgrade(context);
        if let Some(base_url) = about_base_url {
            realm.set_about_base_url(Some(base_url));
        }
        if let Some(document_url) = document_url {
            realm.set_document_url(document_url);
        }
        let proxy = browsing_context::create_context_proxy(ctx, context)
            .map_err(|_| InstallError::Global)?;
        let handle = browsing_context::context_realm_handle(context);
        ctx.set_host_global_this(&handle, proxy.clone())
            .map_err(|_| InstallError::Global)?;
        *realm.window_wrapper.borrow_mut() = ctx.weak_value(&proxy);
        Some(proxy)
    } else {
        None
    };
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
    ctx.get_member(&global, "EventTarget")
        .map_err(|_| InstallError::Global)?;
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
        ("NodeList", ctx.class_constructor::<DomNodeList>()),
        (
            "HTMLCollection",
            ctx.class_constructor::<DomHtmlCollection>(),
        ),
        ("DOMTokenList", ctx.class_constructor::<DomTokenList>()),
        ("CSSStyleDeclaration", ctx.class_constructor::<DomStyle>()),
        ("Element", ctx.class_constructor::<DomElement>()),
        ("HTMLElement", ctx.class_constructor::<DomHtmlElement>()),
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
    let keyboard_constructor = ctx.class_constructor::<ui_events::DomKeyboardEvent>();
    let wheel_constructor = ctx.class_constructor::<ui_events::DomWheelEvent>();
    ui_events::install_keyboard_and_wheel_constants(ctx, &keyboard_constructor, &wheel_constructor)
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
    forms::install(ctx);
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
    if let Some(context) = context.as_ref() {
        browsing_context::bind_context_document(context, &realm);
    }
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
    use lumen_html_text::{DEFAULT_FONT_BYTES, FontFace};
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
    fn native_dom_classes_inherit_and_dispatch_base_methods() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div>x<!--y--></div>", 64).unwrap();
        let value = engine.eval_value("const div = document.querySelector('div'); const text = div.firstChild; const comment = div.lastChild; div instanceof HTMLElement && div instanceof Element && div instanceof Node && div instanceof EventTarget && text instanceof Text && text instanceof CharacterData && text instanceof Node && comment instanceof Comment && document instanceof Document && document instanceof Node && document.nodeType === 9 && text.data === 'x' && Text.prototype instanceof CharacterData && Object.getPrototypeOf(HTMLElement) === Element && document.createDocumentFragment() instanceof DocumentFragment").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
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
                let missing;
                try { main.replaceChild(document.createElement('em'), old); }
                catch (error) { missing = error; }
                return removed === old && removed.parentNode === null &&
                    removedTail === tail && tail.parentNode === null &&
                    replacement.parentNode === main && fragment.childNodes.length === 0 &&
                    main.childNodes.length === 3 && main.childNodes[0] === replacement &&
                    main.childNodes[1].data === 'x' && main.childNodes[2].localName === 'u' &&
                    missing instanceof DOMException && missing.name === 'NotFoundError';
            })()
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
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
        assert!(
            first
                .chunks_exact(4)
                .any(|pixel| pixel == &[255, 0, 0, 255])
        );
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
        assert!(
            while_pending
                .chunks_exact(4)
                .any(|pixel| pixel == &[255, 0, 0, 255])
        );
        assert!(
            !while_pending
                .chunks_exact(4)
                .any(|pixel| pixel == &[0, 0, 255, 255])
        );

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
        assert!(
            second
                .chunks_exact(4)
                .any(|pixel| pixel == &[0, 0, 255, 255])
        );
        assert!(
            !second
                .chunks_exact(4)
                .any(|pixel| pixel == &[255, 0, 0, 255])
        );
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
        assert!(
            !cleared
                .chunks_exact(4)
                .any(|pixel| pixel == &[0, 0, 255, 255])
        );
        assert!(
            !cleared
                .chunks_exact(4)
                .any(|pixel| pixel == &[255, 0, 0, 255])
        );
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
        assert!(
            !target_realm
                .retained_nodes
                .borrow()
                .contains_key(&adopted_node)
        );
        assert!(
            !target_realm
                .script_retentions
                .borrow()
                .contains_key(&adopted_node)
        );
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
        assert!(
            activations[0]
                .dispatch_terminal(engine.ctx(), "load")
                .unwrap()
        );
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
                check('missing href reflects empty string', empty.href === '');
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
        assert!(
            descriptors
                .iter()
                .all(|script| script.parser_inserted && !script.is_async)
        );
        let value = script(
            &mut engine,
            r#"
            document instanceof XMLDocument && document instanceof Document &&
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
        assert!(
            realm
                .dispatch_window_user_agent(engine.ctx(), "load", false, false)
                .unwrap()
        );
        assert!(matches!(
            script(&mut engine, "nativeWindowLoadTrusted === true"),
            Value::Bool(true)
        ));
        assert!(
            realm
                .dispatch_user_agent(engine.ctx(), node, "load", false, false, &[])
                .unwrap()
        );
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
        assert!(
            !realm
                .dispatch_promise_rejection(
                    engine.ctx(),
                    "unhandledrejection",
                    promise.clone(),
                    reason.clone()
                )
                .unwrap()
        );
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
        assert!(
            realm
                .dispatch_promise_rejection(engine.ctx(), "rejectionhandled", promise, reason)
                .unwrap()
        );
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
                check('XMLDocument interface', document instanceof XMLDocument);
                check('Document interface', document instanceof Document);
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
        assert!(
            realm
                .dispatch(engine.ctx(), node, "click", true, true, &[])
                .is_err()
        );
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
        assert!(matches!(
            script(&mut engine, "textarea.value === 'a\\n'"),
            Value::Bool(true)
        ));
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
        assert!(
            !realm
                .dispatch(
                    engine.ctx(),
                    focused,
                    "keydown",
                    true,
                    true,
                    &[("key", Value::str("x"))]
                )
                .unwrap()
        );
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
        assert!(
            realm
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
                ))
        );
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
            adopted === field && blurCount === 1 && document.activeElement === document.body
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
