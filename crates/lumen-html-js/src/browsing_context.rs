//! Native browsing-context and iframe WindowProxy ownership.
//!
//! A URL is only one input to an origin. In particular, initial `about:blank` and
//! `about:srcdoc` documents inherit their creator's origin while keeping their own
//! document URL and inherited base URL.

use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{
    HostRealmScopeError, RealmHandle, WeakValue, WindowProxyDisposition, WindowProxyOperation,
    WindowProxyPolicy,
};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OPAQUE_ORIGIN: AtomicU64 = AtomicU64::new(1);

/// A browser origin with tuple equality or a unique opaque identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum Origin {
    Tuple {
        scheme: String,
        host: String,
        port: Option<u16>,
    },
    Opaque(u64),
}

impl Origin {
    /// The HTML origin algorithm uses the captured source origin for inherited
    /// about documents, rather than a parent's mutable active document.
    fn for_document(url: &str, source: Option<&Self>) -> Self {
        if is_about_blank_url(url) || lumen_common::url::parse(url, None).is_ok_and(|url| url.scheme == "about" && url.path == "srcdoc") {
            if let Some(source) = source { return source.clone(); }
        }
        Self::from_url(url)
    }
    pub fn opaque() -> Self {
        Self::Opaque(NEXT_OPAQUE_ORIGIN.fetch_add(1, Ordering::Relaxed))
    }

    pub fn from_url(url: &str) -> Self {
        let Ok(parsed) = lumen_common::url::parse(url, None) else {
            return Self::opaque();
        };
        if parsed.scheme == "blob" {
            if let Ok(inner) = lumen_common::url::parse(&parsed.path, None) {
                if matches!(
                    inner.kind,
                    lumen_common::url::kind::HTTP
                        | lumen_common::url::kind::HTTPS
                        | lumen_common::url::kind::WS
                        | lumen_common::url::kind::WSS
                        | lumen_common::url::kind::FTP
                ) {
                    return Self::tuple(&inner);
                }
            }
            return Self::opaque();
        }
        if matches!(
            parsed.kind,
            lumen_common::url::kind::HTTP
                | lumen_common::url::kind::HTTPS
                | lumen_common::url::kind::WS
                | lumen_common::url::kind::WSS
                | lumen_common::url::kind::FTP
        ) {
            return Self::tuple(&parsed);
        }
        Self::opaque()
    }

    fn tuple(url: &lumen_common::url::Url) -> Self {
        match url.host.as_deref().filter(|host| !host.is_empty()) {
            Some(host) => Self::Tuple {
                scheme: url.scheme.clone(),
                host: host.to_owned(),
                port: url.port,
            },
            None => Self::opaque(),
        }
    }

    pub fn same_origin(&self, other: &Self) -> bool {
        self == other
    }

    /// The Web-observable serialization: opaque origins serialize as `null`.
    pub fn serialize(&self) -> String {
        match self {
            Self::Opaque(_) => "null".to_owned(),
            Self::Tuple { scheme, host, port } => {
                let mut origin = format!("{scheme}://{host}");
                if let Some(port) = port {
                    origin.push(':');
                    origin.push_str(&port.to_string());
                }
                origin
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameUnsupportedReason {
    SandboxPolicy,
    InvalidSourceUrl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameSource {
    /// A present `srcdoc` attribute, including the empty string.
    SrcDoc(String),
    /// Actual javascript: string completion, retaining the target document URL.
    JavaScriptDocument { source:String, url:String },
    /// A present `src` attribute resolved against the effective document base.
    Url(String),
    /// Neither source attribute is present; the initial document is `about:blank`.
    Blank,
    /// A present `src` could not be parsed by the shared URL implementation.
    InvalidUrl(String),
    /// Attribute processing suppressed a URL equal to an inclusive ancestor's URL.
    Suppressed,
}

impl FrameSource {
    pub(crate) fn retained_inline_bytes(&self)->usize {
        match self {Self::SrcDoc(source)=>source.len(),Self::JavaScriptDocument{source,url}=>source.len().saturating_add(url.len()),_=>0}
    }
}

/// A host-observable snapshot for starting or validating one iframe navigation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameNavigationRequest {
    pub owner: NodeId,
    pub source: FrameSource,
    pub base_url: String,
    pub generation: u64,
    pub unsupported: Option<FrameUnsupportedReason>,
    pub initiator_origin: Origin,
    pub post_resource: Option<Rc<NavigationPostResource>>,
    pub metadata: NavigationMetadata,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UserNavigationInvolvement { #[default] None, Activation, BrowserUi }

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationMetadata {
    pub(crate) policy_container: crate::csp::PolicyContainer,
    pub cookie_source_url: Option<String>,
    pub referrer: lumen_common::referrer::Referrer,
    pub inherited_referrer_policy: lumen_common::referrer::ReferrerPolicy,
    pub source_element: Option<NodeId>,
    pub user_involvement: UserNavigationInvolvement,
}

fn local_policy_url(url: &str) -> bool {
    lumen_common::url::parse_url(url, None).is_some_and(|url| matches!(url.scheme.as_str(), "about" | "data" | "blob"))
}

impl NavigationMetadata {
    pub(crate) fn from_document(document: &DomRealm) -> Self {
        Self { policy_container: document.policy_container(), cookie_source_url: document.cookie_source_url(), referrer: document.navigation_referrer(), source_element: None,
            inherited_referrer_policy: document.referrer_policy.get(),
            user_involvement: UserNavigationInvolvement::None }
    }

    pub(crate) fn from_frame_owner(document: &DomRealm, tree: &lumen_html::Document, owner: NodeId) -> Self {
        let mut metadata = Self::from_document(document);
        metadata.source_element = Some(owner);
        if let Some(policy) = tree.get_attribute_ns_ref(owner, None, "referrerpolicy").ok().flatten()
            .and_then(lumen_common::referrer::ReferrerPolicy::parse) {
            metadata.referrer.policy = policy;
        }
        metadata
    }
}

/// The immutable form resource associated with one history document state.
/// Encoding is performed by the existing form submission algorithm.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NavigationPostResource {
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Bounds shared by every child and staged response in a browsing-context group.
/// Admission measures live documents rather than reserving a full document cap
/// for each small iframe. Hosts can select tighter budgets before author script.
#[derive(Clone, Copy, Debug)]
pub struct FrameResourceLimits {
    pub max_realms: usize,
    pub max_document_nodes: usize,
    pub max_total_nodes: usize,
    pub max_document_source_bytes: usize,
    pub max_total_source_bytes: usize,
}

impl Default for FrameResourceLimits {
    fn default() -> Self {
        Self { max_realms: 64, max_document_nodes: 65_536, max_total_nodes: 262_144,
            max_document_source_bytes: 8 * 1024 * 1024, max_total_source_bytes: 32 * 1024 * 1024 }
    }
}

struct FrameReservation {
    group: std::rc::Weak<ContextGroup>,
    nodes: usize,
    source_bytes: usize,
}

impl FrameReservation {
    fn set_nodes(&mut self, nodes: usize) {
        if let Some(group) = self.group.upgrade() {
            group.staged_nodes.set(group.staged_nodes.get().saturating_sub(self.nodes).saturating_add(nodes));
        }
        self.nodes = nodes;
    }
}

impl Drop for FrameReservation {
    fn drop(&mut self) {
        if let Some(group) = self.group.upgrade() {
            group.staged_realms.set(group.staged_realms.get().saturating_sub(1));
            group.staged_nodes.set(group.staged_nodes.get().saturating_sub(self.nodes));
            group.staged_source_bytes.set(group.staged_source_bytes.get().saturating_sub(self.source_bytes));
        }
    }
}

/// An iframe response whose DOM and host realm are ready but whose stable
/// WindowProxy has not yet been retargeted. The embedder installs its realm
/// providers before committing this token.
pub struct PreparedFrameResponse {
    context: Rc<BrowsingContext>,
    request: FrameNavigationRequest,
    realm: RealmHandle,
    staging_realm: Option<RealmHandle>,
    parsed_document: Option<lumen_html::Document>,
    parser_source: Option<String>,
    details_controller: Rc<super::dialog_popover::DetailsController>,
    document: Option<Rc<DomRealm>>,
    mime: String,
    is_html: bool,
    final_url: String,
    inherited_base: Option<String>,
    metadata: Rc<RealmMetadata>,
    inherits_creator: bool,
    committed: bool,
    reservation: Option<FrameReservation>,
    source_bytes: usize,
}

impl PreparedFrameResponse {
    pub fn reuses_window(&self) -> bool { self.staging_realm.is_some() }
    /// Realm that the trusted embedder should populate before commit.
    pub fn realm_handle(&self) -> RealmHandle {
        self.realm.clone()
    }

    pub fn document_url(&self) -> &str {
        &self.final_url
    }

    pub fn content_type(&self) -> &str {
        &self.mime
    }

    /// The response's trusted origin metadata, independent of its base URL.
    pub fn origin(&self) -> Origin {
        self.metadata.origin.borrow().clone()
    }

    /// Trusted transport metadata captured when a Blob fetch began. Capturing
    /// this before byte reads preserves the origin if that URL is revoked later.
    pub fn set_blob_origin(&mut self, origin: Origin) -> Result<(), FrameInstallError> {
        if self.document.is_some() || self.committed
            || !lumen_common::url::parse(&self.final_url, None).is_ok_and(|url| url.scheme == "blob") {
            return Err(FrameInstallError::HostRealm);
        }
        self.metadata.origin.replace(origin);
        Ok(())
    }

    /// Fallback base before any parsed `<base href>` is applied.
    pub fn fallback_base_url(&self) -> &str {
        self.inherited_base.as_deref().unwrap_or(&self.final_url)
    }

    /// The installed native document, if DOM initialization has completed.
    pub fn document_realm(&self) -> Option<Rc<DomRealm>> {
        self.document.clone()
    }

    /// Discard an uncommitted response. The embedder must cancel its queued work
    /// and dispose the returned realms after leaving them.
    pub fn discard(self, ctx: &mut Ctx) -> Vec<RealmHandle> {
        if self.committed {
            return Vec::new();
        }
        let mut retired = Vec::new();
        if let Some(document) = self.document {
            super::navigation_lifecycle::destroy(ctx, &document);
            document.retire_all_frame_contexts(ctx, &mut retired);
        }
        let realm=if let Some(staging)=self.staging_realm {staging}else{self.realm};
        // Preparation can register an owner before a Document exists. This
        // realm is actually discarded, so its unqualified owners and tasks
        // must retire along with document-qualified work.
        super::scheduling::cancel_tasks_for_realm(ctx,&realm);
        if let Some(timers)=ctx.host_mut::<lumen_timers::Timers>() {timers.cancel_realm(&realm);}
        retired.push(realm);
        retired
    }
}

/// Results of publishing a prepared iframe response. The host must cancel
/// tasks for and dispose `retired_realms` after it has left those realms.
pub struct FrameCommitResult {
    pub document: Rc<DomRealm>,
    pub retired_realms: Vec<RealmHandle>,
}

#[derive(Debug)]
pub enum FrameInstallError {
    StaleRequest,
    WrongFrame,
    AlreadyCommitted,
    NotInitialized,
    Unsupported(FrameUnsupportedReason),
    UnsupportedContentType(String),
    Document(lumen_html::Error),
    Parse(InstallError),
    HostRealm,
    ResourceLimit(String),
}

pub(crate) struct RealmMetadata {
    key: Cell<usize>,
    origin: RefCell<Origin>,
    self_proxy: RefCell<Option<WeakValue>>,
    parent_proxy: RefCell<Option<WeakValue>>,
    top_proxy: RefCell<Option<WeakValue>>,
    parent_metadata: Option<std::rc::Weak<RealmMetadata>>,
    owner_realm: Option<std::rc::Weak<DomRealm>>,
    owner_node: Option<NodeId>,
}

#[derive(Default)]
struct RealmMetadataRegistry {
    realms: RefCell<HashMap<usize, std::rc::Weak<RealmMetadata>>>,
}

impl RealmMetadataRegistry {
    fn register(&self, metadata: &Rc<RealmMetadata>) {
        let key = metadata.key.get();
        let mut realms = self.realms.borrow_mut();
        // Navigation can prepare and discard many realms without ever reading
        // their metadata again. Sweep dead weak keys at this write boundary,
        // while preserving entries retained by old live realms/functions.
        realms.retain(|_, known| known.strong_count() != 0);
        realms.insert(key, Rc::downgrade(metadata));
    }

    fn for_realm(&self, realm: &RealmHandle) -> Option<Rc<RealmMetadata>> {
        let key = realm.global().object_identity()?;
        let metadata = {
            let realms = self.realms.borrow();
            realms.get(&key)?.upgrade()
        };
        if metadata
            .as_ref()
            .is_some_and(|metadata| metadata.key.get() == key)
        {
            return metadata;
        }
        self.realms.borrow_mut().remove(&key);
        None
    }
}

struct ContextGroup {
    allocation_budget_group: Rc<()>,
    embedded_pixels:std::sync::Arc<lumen_common::limits::ByteBudget>,
    history: Rc<super::history::HistoryStore>,
    /// Strong ownership here is intentional: only active navigables belong to the host group.
    /// Detached/replaced contexts are removed and their realm handles are then released.
    contexts: RefCell<HashMap<usize, Rc<BrowsingContext>>>,
    metadata: Rc<RealmMetadataRegistry>,
    root_key: usize,
    next_insertion: Cell<u64>,
    transition_generation: Cell<u64>,
    limits: Cell<FrameResourceLimits>,
    staged_realms: Cell<usize>,
    staged_nodes: Cell<usize>,
    staged_source_bytes: Cell<usize>,
    documents: RefCell<Vec<FrameDocumentRecord>>,
    pending_retired: RefCell<Vec<RealmHandle>>,
}

struct FrameDocumentRecord {
    document: std::rc::Weak<DomRealm>,
    document_id: NodeId,
    nodes: Rc<Cell<usize>>,
    source_bytes: usize,
}

impl ContextGroup {
    fn new(root_key: usize) -> Rc<Self> {
        Rc::new(Self {
            allocation_budget_group: Rc::new(()),
            embedded_pixels:lumen_common::limits::ByteBudget::new(64*1024*1024),
            history: Rc::new(super::history::HistoryStore::default()),
            contexts: RefCell::new(HashMap::new()),
            metadata: Rc::new(RealmMetadataRegistry::default()),
            root_key,
            next_insertion: Cell::new(1),
            transition_generation: Cell::new(0),
            limits: Cell::new(FrameResourceLimits::default()),
            staged_realms: Cell::new(0),
            staged_nodes: Cell::new(0),
            staged_source_bytes: Cell::new(0),
            documents: RefCell::new(Vec::new()),
            pending_retired: RefCell::new(Vec::new()),
        })
    }

    fn register(&self, context: &Rc<BrowsingContext>) {
        let mut histories = self.history.contexts.borrow_mut();
        histories.retain(|_, context| context.strong_count() != 0);
        histories.insert(context.history_id(), Rc::downgrade(context));
        drop(histories);
        let key = context.realm.borrow().global().object_identity();
        if let Some(key) = key {
            let metadata = context.metadata.borrow().clone();
            metadata.key.set(key);
            self.metadata.register(&metadata);
            self.contexts.borrow_mut().insert(key, context.clone());
        }
    }

    fn admit(self: &Rc<Self>, ctx: &mut Ctx, source_bytes: usize) -> OpResult<(FrameReservation, usize)> {
        match self.admit_without_collection(source_bytes) {
            Err(error) if error.class() == "QuotaExceededError"
                && source_bytes <= self.limits.get().max_document_source_bytes => {
                // Retired realms remain available to reachable author references. Reclaim only
                // unreachable cycles before deciding the bounded profile really is exhausted.
                // This is admission pressure, not a collection on every frame or navigation.
                let has_retired = self.documents.borrow().iter().any(|record|
                    record.document.upgrade().is_some_and(|document| document.lifecycle.destroyed.get()));
                if !has_retired { return Err(error); }
                ctx.collect_garbage();
                self.admit_without_collection(source_bytes)
            }
            result => result,
        }
    }

    fn admit_without_collection(self: &Rc<Self>, source_bytes: usize) -> OpResult<(FrameReservation, usize)> {
        let limits = self.limits.get();
        let contexts = self.contexts.borrow();
        let mut nodes = self.staged_nodes.get();
        let mut bytes = self.staged_source_bytes.get();
        let mut counted_documents = HashSet::new();
        let mut realms = self.staged_realms.get();
        {
            let mut documents = self.documents.borrow_mut();
            documents.retain(|record| record.document.strong_count() != 0);
            for record in documents.iter() {
                let Some(document) = record.document.upgrade() else { continue; };
                counted_documents.insert(Rc::as_ptr(&document) as usize);
                let session = document.session.try_borrow()
                    .map_err(|_| OpError::new("InvalidStateError", "Frame admission requires an available document"))?;
                nodes = nodes.saturating_add(session.document().node_count());
                bytes = bytes.saturating_add(record.source_bytes);
                realms = realms.saturating_add(1);
            }
        }
        for context in contexts.values() {
            if let Some(document) = context.document() {
                if !counted_documents.insert(Rc::as_ptr(&document) as usize) { continue; }
                let session = document.session.try_borrow()
                    .map_err(|_| OpError::new("InvalidStateError", "Frame admission requires an available document"))?;
                nodes = nodes.saturating_add(session.document().node_count());
            }
            bytes = bytes.saturating_add(context.source_bytes.get());
            realms = realms.saturating_add(1);
        }
        if realms >= limits.max_realms || source_bytes > limits.max_document_source_bytes
            || bytes.saturating_add(source_bytes) > limits.max_total_source_bytes
            || nodes >= limits.max_total_nodes {
            return Err(OpError::new("QuotaExceededError", "Browsing-context group resource budget exceeded"));
        }
        let max_nodes = limits.max_document_nodes.min(limits.max_total_nodes - nodes);
        self.staged_realms.set(self.staged_realms.get() + 1);
        self.staged_source_bytes.set(self.staged_source_bytes.get() + source_bytes);
        Ok((FrameReservation { group: Rc::downgrade(self), nodes: 0, source_bytes }, max_nodes))
    }

    fn track_document(self: &Rc<Self>, document: &Rc<DomRealm>, source_bytes: usize) {
        let mut documents = self.documents.borrow_mut();
        documents.retain(|record| record.document.strong_count() != 0);
        let weak = Rc::downgrade(document);
        if let Some(record) = documents.iter_mut().find(|record| record.document.ptr_eq(&weak)) { record.source_bytes = source_bytes; }
        else {
            let mut session = document.session.borrow_mut();
            let nodes = Rc::new(Cell::new(session.document().node_count()));
            session.document_mut().set_node_count_tracking(nodes.clone());
            documents.push(FrameDocumentRecord { document: weak, document_id: session.document().root(), nodes, source_bytes });
        }
        drop(documents);
        let group = Rc::downgrade(self);
        document.session.borrow_mut().document_mut().set_allocation_budget_group(self.allocation_budget_group.clone());
        document.session.borrow_mut().document_mut().set_allocation_budget(Some(Rc::new(move |current, required, aggregate_growth| {
            let Some(group) = group.upgrade() else { return Ok(()); };
            let limits = group.limits.get();
            let local_count = current.node_count().checked_add(required).ok_or(lumen_html::Error::LimitExceeded)?;
            if local_count > limits.max_document_nodes { return Err(lumen_html::Error::LimitExceeded); }
            let mut count = current.node_count().checked_add(aggregate_growth).ok_or(lumen_html::Error::LimitExceeded)?;
            count = count.saturating_add(group.staged_nodes.get());
            let records = group.documents.try_borrow().map_err(|_| lumen_html::Error::LimitExceeded)?;
            for record in records.iter() {
                if record.document_id == current.root() { continue; }
                if record.document.strong_count() != 0 { count = count.saturating_add(record.nodes.get()); }
            }
            if count > limits.max_total_nodes { return Err(lumen_html::Error::LimitExceeded); }
            Ok(())
        })));
    }

    fn unregister_active(&self, realm: &RealmHandle) {
        if let Some(key) = realm.global().object_identity() {
            self.contexts.borrow_mut().remove(&key);
        }
    }

    fn for_realm(&self, realm: &RealmHandle) -> Option<Rc<BrowsingContext>> {
        let key = realm.global().object_identity()?;
        self.contexts.borrow().get(&key).cloned()
    }
}

#[derive(Default)]
struct ContextGroupRegistry {
    groups: HashMap<usize, Rc<ContextGroup>>,
}

fn group_for_root(ctx: &mut Ctx, realm: &RealmHandle) -> Rc<ContextGroup> {
    let key = realm
        .global()
        .object_identity()
        .expect("a browsing context has an object global");
    if !ctx.op_state().has::<ContextGroupRegistry>() {
        ctx.op_state().put(ContextGroupRegistry::default());
    }
    ctx.op_state()
        .get_mut::<ContextGroupRegistry>()
        .expect("the context-group registry was just installed")
        .groups
        .entry(key)
        .or_insert_with(|| ContextGroup::new(key))
        .clone()
}

fn remove_group(ctx: &mut Ctx, root_key: usize) {
    if let Some(registry) = ctx.op_state().get_mut::<ContextGroupRegistry>() {
        registry.groups.remove(&root_key);
    }
}

/// Per-navigable browser state. The document points back weakly, while this
/// object retains the active document and its host realm for child contexts.
pub(crate) struct BrowsingContext {
    pub(crate) history: RefCell<Option<Rc<super::history::NavigableHistory>>>,
    history_navigation: RefCell<Option<Rc<super::history::Entry>>>,
    committed_history_entry: RefCell<Option<Rc<super::history::Entry>>>,
    pending_post_resource: RefCell<Option<Rc<NavigationPostResource>>>,
    replace_navigation: Cell<bool>,
    group: std::rc::Weak<ContextGroup>,
    metadata: RefCell<Rc<RealmMetadata>>,
    proxy_metadata: Rc<RefCell<Rc<RealmMetadata>>>,
    realm: RefCell<RealmHandle>,
    window_proxy: RefCell<Option<WeakValue>>,
    document: RefCell<Option<Rc<DomRealm>>>,
    parent: Option<std::rc::Weak<BrowsingContext>>,
    opener: RefCell<Option<std::rc::Weak<BrowsingContext>>>,
    owner_realm: Option<std::rc::Weak<DomRealm>>,
    owner_node: Option<NodeId>,
    inherit_about_origin: Cell<bool>,
    /// A Location API navigation takes precedence over reflected `src`/`srcdoc`
    /// until one of those author-controlled iframe inputs changes.
    location_navigation: RefCell<Option<String>>,
    javascript_document: RefCell<Option<(String,String)>>,
    navigation_initiator: RefCell<Option<Origin>>,
    navigation_initiator_base: RefCell<Option<String>>,
    navigation_metadata: RefCell<Option<NavigationMetadata>>,
    active: Cell<bool>,
    navigation_generation: Cell<u64>,
    beforeunload_generation: Cell<Option<u64>>,
    ignored_attribute_navigation: Cell<Option<u64>>,
    initial_blank_load_dispatched: Cell<bool>,
    insertion_order: u64,
    target_name: RefCell<String>,
    source_bytes: Cell<usize>,
}

struct BrowsingContextService {
    context: std::rc::Weak<BrowsingContext>,
    metadata: Rc<RealmMetadata>,
    registry: Rc<RealmMetadataRegistry>,
}

/// A live iframe browsing context returned to the trusted embedder.
#[derive(Clone)]
pub struct FrameContext {
    inner: Rc<BrowsingContext>,
}

/// Non-owning identity for a frame, independent of its owner node and URL.
/// Queued host tasks can reject a replacement context without retaining its
/// document, scripts or resource caches.
#[derive(Clone)]
pub struct WeakFrameIdentity {
    inner: std::rc::Weak<BrowsingContext>,
}

impl WeakFrameIdentity {
    pub fn matches(&self, frame: &FrameContext) -> bool {
        self.inner.ptr_eq(&Rc::downgrade(&frame.inner))
    }

    /// Live providers measure the owner without retaining a context/document cycle.
    pub fn content_viewport_size(&self) -> OpResult<Option<(u32, u32)>> {
        self.inner.upgrade().filter(|context| context.active.get())
            .map(|inner| FrameContext { inner }.content_viewport_size()).transpose()
    }
}

impl FrameContext {
    pub fn context_identity(&self)->usize {Rc::as_ptr(&self.inner) as usize}
    pub fn set_navigation_post_resource(&self, request: &FrameNavigationRequest, headers: Vec<(String, String)>, body: Vec<u8>) -> OpResult<()> {
        if !self.request_is_current(request) { return Err(OpError::new("InvalidStateError", "superseded POST navigation")); }
        let resource = Rc::new(NavigationPostResource { headers, body });
        super::history::preflight_post_resource(&self.inner, &resource)?;
        *self.inner.pending_post_resource.borrow_mut() = Some(resource);
        Ok(())
    }
    pub fn prepare_document_deactivation(&self, ctx: &mut Ctx) -> OpResult<()> { super::history::before_deactivation(ctx, &self.inner) }
    pub fn is_top_level(&self) -> bool { self.inner.parent.is_none() }
    pub fn is_primary_top_level(&self)->bool {self.is_top_level() && self.inner.insertion_order==0}

    /// Complete a traversal fetch that produced no document (HTTP 204/205).
    /// The target entry becomes current while its predecessor remains active.
    pub fn finish_navigation_without_document(&self, ctx: &mut Ctx) -> OpResult<()> {
        let target = self.inner.history_navigation.borrow_mut().take();
        *self.inner.committed_history_entry.borrow_mut() = target.clone();
        super::history::navigation_finished(ctx, &self.inner, target, None, self.navigation_request().source, false)
    }
    pub(crate) fn lifecycle_documents(&self, ctx: &mut Ctx) -> OpResult<Vec<(Rc<DomRealm>, RealmHandle)>> {
        let mut contexts = std::collections::VecDeque::from([self.clone()]);
        let mut documents = Vec::new();
        while let Some(frame) = contexts.pop_front() {
            let Some(document) = frame.current_document() else { continue; };
            if !is_active_document(&frame.inner, &document) { continue; }
            contexts.extend(document.frame_contexts(ctx)?);
            documents.push((document, frame.realm_handle()));
        }
        Ok(documents)
    }

    pub fn queue_beforeunload(&self, ctx: &mut Ctx) -> OpResult<super::navigation_lifecycle::NavigationPhase> {
        let generation = self.navigation_generation();
        if self.inner.beforeunload_generation.replace(Some(generation)) == Some(generation) {
            return super::navigation_lifecycle::queue_phase(ctx, Vec::new(), false);
        }
        let documents = self.lifecycle_documents(ctx)?;
        super::navigation_lifecycle::queue_phase(ctx, documents, false)
    }

    pub fn queue_document_unload(&self, ctx: &mut Ctx) -> OpResult<super::navigation_lifecycle::NavigationPhase> {
        let mut documents = self.lifecycle_documents(ctx)?;
        documents.reverse();
        super::navigation_lifecycle::queue_phase(ctx, documents, true)
    }
    pub fn weak_identity(&self) -> WeakFrameIdentity {
        WeakFrameIdentity {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub(crate) fn embedded_pixel_budget(&self)->OpResult<std::sync::Arc<lumen_common::limits::ByteBudget>> {
        self.inner.group.upgrade().map(|group|group.embedded_pixels.clone())
            .ok_or_else(||OpError::new("InvalidStateError","Embedded browsing-context group is retired"))
    }

    pub fn owner_node(&self) -> NodeId {
        self.inner
            .owner_node
            .unwrap_or_else(|| self.inner.document().expect("active top-level document").session.borrow().document().root())
    }

    /// Measure the iframe's current content-box viewport after refreshing its
    /// owner document's layout. CSS borders and padding are excluded, and an
    /// owner with no rendered box has a zero-sized viewport.
    pub fn content_viewport_size(&self) -> OpResult<(u32, u32)> {
        let owner = self
            .inner
            .owner_realm
            .as_ref()
            .and_then(std::rc::Weak::upgrade)
            .ok_or_else(|| OpError::new("InvalidStateError", "iframe owner document is gone"))?;
        synchronize_frame_media_environment(&owner)?;
        owner.flush_layout()?;
        self.content_viewport_size_after_layout()
    }

    /// The renderer has committed owner layout and holds no Session borrow.
    pub(crate) fn content_viewport_size_after_layout(&self)->OpResult<(u32,u32)> {
        let owner=self.inner.owner_realm.as_ref().and_then(std::rc::Weak::upgrade)
            .ok_or_else(||OpError::new("InvalidStateError","iframe owner document is gone"))?;
        let session_handle = owner.session_handle();
        let size = {
            let mut session = session_handle.borrow_mut();
            crate::geometry::content_box_size(&mut session, self.owner_node())
        }
        .unwrap_or((0.0, 0.0));
        let round_dimension = |value: f32| {
            if !value.is_finite() || value < 0.0 || value.round() > u32::MAX as f32 {
                return Err(OpError::new(
                    "InvalidStateError",
                    "iframe content viewport dimensions are invalid",
                ));
            }
            Ok(value.round() as u32)
        };
        Ok((round_dimension(size.0)?, round_dimension(size.1)?))
    }

    pub fn origin(&self) -> Origin {
        self.inner.metadata.borrow().origin.borrow().clone()
    }

    pub fn origin_serialization(&self) -> String {
        self.origin().serialize()
    }

    pub fn realm_handle(&self) -> RealmHandle {
        self.inner.realm.borrow().clone()
    }

    /// The container's document qualifies NodeId for host lifecycle bookkeeping.
    pub fn owner_document(&self) -> Option<Rc<DomRealm>> {
        if self.is_top_level() { self.current_document() } else { self.inner.owner_realm.as_ref()?.upgrade() }
    }

    pub fn target_name(&self) -> String { self.inner.target_name() }

    pub fn top_document(&self) -> Option<Rc<DomRealm>> { self.inner.top_context().document() }
    pub fn parent_navigation_context(&self) -> Option<FrameContext> { self.inner.parent_context().map(|inner| FrameContext { inner }) }

    pub fn current_document(&self) -> Option<Rc<DomRealm>> {
        self.inner
            .is_active()
            .then(|| self.inner.document.borrow().clone())
            .flatten()
    }

    pub fn window_proxy(&self) -> Option<Value> {
        self.inner
            .window_proxy
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
    }

    pub fn navigation_request(&self) -> FrameNavigationRequest {
        request_for_frame(&self.inner)
    }

    /// Request a host-controlled navigation for this iframe without changing
    /// its reflected `src` or `srcdoc` attributes. The returned snapshot carries
    /// the new generation so a host can reject a response after later source,
    /// base, or ownership mutations.
    pub fn request_host_navigation(&self, input: &str) -> OpResult<FrameNavigationRequest> {
        self.inner.replace_navigation.set(false);
        let entry_base_url = self
            .inner
            .document()
            .map(|document| document.base_url())
            .unwrap_or_else(|| self.inner.current_document_url());
        self.inner
            .request_location_navigation_from(input, &entry_base_url)?;
        Ok(self.navigation_request())
    }

    /// Capture the active submitting document's origin and base before the
    /// host begins an asynchronous navigation of a different target context.
    pub fn request_host_navigation_from(&self, input: &str, source: &Rc<DomRealm>) -> OpResult<FrameNavigationRequest> {
        let source_context = source.browsing_context()
            .filter(|context| is_active_document(context, source) && context.group.ptr_eq(&self.inner.group))
            .ok_or_else(|| OpError::new("InvalidStateError", "navigation source document is not fully active"))?;
        let origin = source_context.root_or_child_origin();
        let base_url = source.base_url();
        self.inner.replace_navigation.set(false);
        self.inner.request_location_navigation_from(input, &base_url)?;
        *self.inner.navigation_initiator.borrow_mut() = Some(origin);
        *self.inner.navigation_metadata.borrow_mut() = Some(NavigationMetadata::from_document(source));
        Ok(self.navigation_request())
    }

    /// The object algorithm has already fetched this response. Capture its
    /// source settings and replacement history handling before installation.
    pub fn request_object_response(&self, url: &str, source: &Rc<DomRealm>) -> OpResult<FrameNavigationRequest> {
        let request = self.request_host_navigation_from(url, source)?;
        self.inner.replace_navigation.set(true);
        let mut metadata = request.metadata;
        metadata.source_element = self.inner.owner_node;
        *self.inner.navigation_metadata.borrow_mut() = Some(metadata);
        Ok(self.navigation_request())
    }

    /// Check a captured fetch request before installing its response. A later
    /// source or ownership mutation invalidates an older request without retargeting the
    /// WindowProxy until a current response is ready.
    pub fn request_is_current(&self, request: &FrameNavigationRequest) -> bool {
        self.inner.is_active() && (request == &self.navigation_request()
            || (self.inner.ignored_attribute_navigation.get() == Some(request.generation)
                && self.navigation_generation() == request.generation))
    }

    /// Install an immutable source snapshot captured during form submission,
    /// rather than reading the source document again when the host starts it.
    pub fn set_navigation_metadata(&self, request: &FrameNavigationRequest, metadata: NavigationMetadata) -> OpResult<FrameNavigationRequest> {
        if !self.request_is_current(request) { return Err(OpError::new("InvalidStateError", "navigation request is stale")); }
        *self.inner.navigation_metadata.borrow_mut() = Some(metadata);
        Ok(self.navigation_request())
    }

    pub fn navigation_generation(&self) -> u64 {
        self.inner.navigation_generation.get()
    }

    /// True after the synchronous post-connection load steps for the initial
    /// about:blank document have run. Later navigations still queue their own
    /// iframe load event.
    pub fn initial_blank_load_dispatched(&self) -> bool {
        self.inner.initial_blank_load_dispatched.get()
    }

    /// Claim the one synchronous initial-blank load event for a connected
    /// iframe. This is limited to a generation-zero blank source; srcdoc and
    /// real URL navigations complete through their normal load path.
    pub(crate) fn claim_initial_blank_load(&self) -> bool {
        if !self.inner.active.get() || self.navigation_generation() != 0 {
            return false;
        }
        let request = self.navigation_request();
        if !matches!(&request.source, FrameSource::Blank)
            && !matches!(&request.source, FrameSource::Url(url) if is_about_blank_url(url)) {
            return false;
        }
        if let FrameSource::Url(url) = request.source {
            if let Some(document) = self.current_document() { document.set_same_document_url(url); }
        }
        !self.inner.initial_blank_load_dispatched.replace(true)
    }

    pub fn install_response_for_request(
        &self,
        ctx: &mut Ctx,
        request: &FrameNavigationRequest,
        final_url: &str,
        content_type: &str,
        source: &str,
        max_nodes: usize,
    ) -> Result<Rc<DomRealm>, FrameInstallError> {
        let mut prepared = self.prepare_response_for_request(
            ctx,
            request,
            final_url,
            content_type,
            source,
            max_nodes,
        )?;
        if let Err(error) = self.initialize_prepared_document(ctx, &mut prepared) {
            for realm in prepared.discard(ctx) {
                dispose_compat_realm(ctx, &realm);
            }
            return Err(error);
        }
        let committed = match self.commit_prepared_response(ctx, &mut prepared) {
            Ok(committed) => committed,
            Err(error) => {
                for realm in prepared.discard(ctx) {
                    dispose_compat_realm(ctx, &realm);
                }
                return Err(error);
            }
        };
        for realm in committed.retired_realms {
            dispose_compat_realm(ctx, &realm);
        }
        // This compatibility entry point installs a parsed DOM, without a
        // resource/script host. Browser embedders use the staged transaction
        // and drive each parser yield with their real resource continuation.
        ctx.with_host_realm(&self.realm_handle(), |ctx| {
            while committed.document.has_live_document_parser() {
                committed.document.next_document_parser_script(ctx).map_err(|_| FrameInstallError::HostRealm)?;
            }
            Ok::<_, FrameInstallError>(())
        }).map_err(|_| FrameInstallError::HostRealm)??;
        Ok(committed.document)
    }

    /// Parse a response and create a fresh, unpublished realm. The embedder
    /// installs runtime providers first, calls `initialize_prepared_document`
    /// to install the DOM against those providers, then commits the result.
    pub fn prepare_response_for_request(
        &self,
        ctx: &mut Ctx,
        request: &FrameNavigationRequest,
        final_url: &str,
        content_type: &str,
        source: impl Into<String>,
        max_nodes: usize,
    ) -> Result<PreparedFrameResponse, FrameInstallError> {
        self.prepare_response_for_request_inner(ctx, request, final_url, content_type, source, max_nodes, false)
    }

    /// Failed navigation installs a fresh opaque error document. It follows
    /// ordinary parser completion and container load steps without fetching
    /// the denied target or reusing an initial same-origin Window.
    pub fn prepare_failed_response_for_request(
        &self, ctx: &mut Ctx, request: &FrameNavigationRequest,
        final_url: &str, max_nodes: usize,
    ) -> Result<PreparedFrameResponse, FrameInstallError> {
        self.prepare_response_for_request_inner(ctx, request, final_url, "text/html",
            "<!doctype html><body></body>", max_nodes, true)
    }

    fn prepare_response_for_request_inner(
        &self,
        ctx: &mut Ctx,
        request: &FrameNavigationRequest,
        final_url: &str,
        content_type: &str,
        source: impl Into<String>,
        max_nodes: usize,
        failed_navigation: bool,
    ) -> Result<PreparedFrameResponse, FrameInstallError> {
        if !self.inner.active.get() || !self.request_is_current(request) {
            return Err(FrameInstallError::StaleRequest);
        }
        if let Some(reason) = &request.unsupported {
            return Err(FrameInstallError::Unsupported(reason.clone()));
        }
        super::history::navigable(&self.inner).map_err(|_| FrameInstallError::HostRealm)?;
        super::history::preflight_navigation(&self.inner, final_url, &request.source, self.inner.replace_navigation.get())
            .map_err(|error| FrameInstallError::ResourceLimit(format!("{error:?}")))?;

        let group = self
            .inner
            .group
            .upgrade()
            .ok_or(FrameInstallError::HostRealm)?;
        let _proxy = self.window_proxy().ok_or(FrameInstallError::HostRealm)?;
        let source = source.into();
        let source_bytes = source.len();
        let (mut reservation, admitted_nodes) = group.admit(ctx, source.len())
            .map_err(|error| FrameInstallError::ResourceLimit(format!("{error:?}")))?;
        let max_nodes = max_nodes.min(admitted_nodes);
        let new_realm = ctx.create_host_realm();
        let parsed = ctx.with_host_realm(&new_realm, |ctx| {
            let controller = super::dialog_popover::DetailsController::prepare(ctx)
                .map_err(|_| FrameInstallError::HostRealm)?;
            let document = parse_frame_document(source, content_type, final_url, max_nodes, &controller)?;
            Ok::<_, FrameInstallError>((document, controller))
        });
        let ((mime, document, is_html, parser_source), details_controller) = match parsed {
            Ok(Ok(parsed)) => parsed,
            Ok(Err(error)) => {
                dispose_compat_realm(ctx, &new_realm);
                return Err(error);
            }
            Err(_) => {
                dispose_compat_realm(ctx, &new_realm);
                return Err(FrameInstallError::HostRealm);
            }
        };
        reservation.set_nodes(document.node_count());
        let explicit_about_blank = matches!(
            &request.source,
            FrameSource::Url(source_url)
                if is_about_blank_url(source_url) && is_about_blank_url(final_url)
        );
        let inherits_creator =
            matches!(request.source, FrameSource::Blank | FrameSource::SrcDoc(_) | FrameSource::JavaScriptDocument {..})
                || explicit_about_blank;
        let historical = self.inner.history_navigation.borrow().clone();
        let origin = if failed_navigation {
            Origin::opaque()
        } else if inherits_creator {
            let source = historical.as_ref().map_or_else(|| request.initiator_origin.clone(), |entry| entry.document_state().origin.borrow().clone());
            Origin::for_document(final_url, Some(&source))
        } else {
            crate::object_urls::origin(ctx, final_url).unwrap_or_else(|| Origin::from_url(final_url))
        };
        let new_metadata = self.inner.new_realm_metadata(ctx, &new_realm, origin);
        let reuse_window = self.current_document().is_some_and(|document| document.lifecycle.initial_about_blank.get())
            && self.inner.root_or_child_origin().same_origin(&metadata_origin(&new_metadata));
        let (new_realm, staging_realm, new_metadata) = if reuse_window {
            let active = self.realm_handle();
            let metadata = self.inner.new_realm_metadata(ctx, &active, metadata_origin(&new_metadata));
            (active, Some(new_realm), metadata)
        } else { (new_realm, None, new_metadata) };
        let inherited_base = inherits_creator.then(|| {
            if let Some(entry) = &historical { return entry.document_state().base_url.clone(); }
            request.base_url.clone()
        });
        Ok(PreparedFrameResponse {
            context: self.inner.clone(),
            request: request.clone(),
            realm: new_realm,
            staging_realm,
            parsed_document: Some(document),
            parser_source,
            details_controller,
            document: None,
            mime,
            is_html,
            final_url: final_url.to_owned(),
            inherited_base,
            metadata: new_metadata,
            inherits_creator,
            committed: false,
            reservation: Some(reservation),
            source_bytes,
        })
    }

    /// Install native DOM bindings into a prepared realm after the host has
    /// installed that realm's runtime providers. This does not publish the
    /// realm or retarget the stable WindowProxy.
    pub fn initialize_prepared_document(
        &self,
        ctx: &mut Ctx,
        prepared: &mut PreparedFrameResponse,
    ) -> Result<Rc<DomRealm>, FrameInstallError> {
        if prepared.committed {
            return Err(FrameInstallError::AlreadyCommitted);
        }
        if !Rc::ptr_eq(&self.inner, &prepared.context) {
            return Err(FrameInstallError::WrongFrame);
        }
        if let Some(document) = &prepared.document {
            return Ok(document.clone());
        }
        if !self.inner.is_active() || !self.request_is_current(&prepared.request) {
            return Err(FrameInstallError::StaleRequest);
        }
        let document = prepared
            .parsed_document
            .take()
            .ok_or(FrameInstallError::HostRealm)?;
        let staged_context = self.inner.clone();
        let metadata = prepared.metadata.clone();
        let mime = prepared.mime.clone();
        let final_url = prepared.final_url.clone();
        let inherited_base = prepared.inherited_base.clone();
        let is_html = prepared.is_html;
        let details_controller = prepared.details_controller.clone();
        let reuse_window = prepared.reuses_window();
        let result = ctx.with_host_realm(&prepared.realm, |ctx| {
            super::install_document_staged(
                ctx,
                document,
                &mime,
                is_html,
                staged_context,
                metadata,
                final_url,
                inherited_base,
                details_controller,
                reuse_window,
            )
        });
        let document = match result {
            Ok(Ok(document)) => document,
            Ok(Err(error)) => return Err(FrameInstallError::Parse(error)),
            _ => return Err(FrameInstallError::HostRealm),
        };
        if let (Some(parent),Some(node))=(self.inner.parent.as_ref().and_then(std::rc::Weak::upgrade),self.inner.owner_node) {
            let owner=parent.document.borrow().clone();
            if let Some(owner)=owner {
                document.initialize_permissions_policy(&owner,Some(node)).map_err(|_|FrameInstallError::HostRealm)?;
                document.inherit_embedding_color_scheme(&owner,node).map_err(|_|FrameInstallError::HostRealm)?;
            }
        }
        if let Some(container) = crate::object_urls::policy_container(ctx, &prepared.final_url) {
            document.inherit_policy_container(&container);
        } else if let Some(entry) = self.inner.history_navigation.borrow().as_ref() {
            if local_policy_url(&prepared.final_url) {
                document.inherit_policy_container(&entry.document_state().policy_container.borrow());
            }
        } else if matches!(prepared.request.source, FrameSource::SrcDoc(_) | FrameSource::Blank | FrameSource::JavaScriptDocument {..})
            || local_policy_url(&prepared.final_url) {
            document.inherit_policy_container(&prepared.request.metadata.policy_container);
        }
        if matches!(prepared.request.source, FrameSource::SrcDoc(_) | FrameSource::Blank | FrameSource::JavaScriptDocument {..})
            || prepared.final_url == "about:blank" {
            *document.cookie_source_url.borrow_mut() = prepared.request.metadata.cookie_source_url.clone();
        }
        prepared.document = Some(document.clone());
        if let Some(group) = self.inner.group.upgrade() {
            group.track_document(&document, prepared.source_bytes);
        }
        // Native wrappers can retain an initialized document independently of
        // this transaction. Its weak record now owns the admission accounting.
        drop(prepared.reservation.take());
        if let Some(source) = prepared.parser_source.take() {
            document.set_document_parser_source(source).map_err(|_| FrameInstallError::HostRealm)?;
        }
        Ok(document)
    }

    /// Publish a prepared response if its captured request is still current.
    /// On error the token remains available to `PreparedFrameResponse::discard`.
    pub fn commit_prepared_response(
        &self,
        ctx: &mut Ctx,
        prepared: &mut PreparedFrameResponse,
    ) -> Result<FrameCommitResult, FrameInstallError> {
        if prepared.committed {
            return Err(FrameInstallError::AlreadyCommitted);
        }
        if !Rc::ptr_eq(&self.inner, &prepared.context) {
            return Err(FrameInstallError::WrongFrame);
        }
        if !self.inner.active.get() || !self.request_is_current(&prepared.request) {
            return Err(FrameInstallError::StaleRequest);
        }
        let document = prepared
            .document
            .as_ref()
            .ok_or(FrameInstallError::NotInitialized)?
            .clone();

        // SVG 2 §3.10: embedded SVG preserves its opacity when composited
        // into the parent; only a top-level SVG page uses the white backdrop.
        // The parser is still live at commit; its documentElement may not exist
        // yet. The prepared SVG response type already establishes the canvas.
        if self.inner.parent.is_some()
            && (prepared.mime == "image/svg+xml" || document.session.borrow().embedded_document_is_svg())
        {
            document.session.borrow_mut().set_canvas_background(None);
        }

        let group = self
            .inner
            .group
            .upgrade()
            .ok_or(FrameInstallError::HostRealm)?;
        let proxy = self.window_proxy().ok_or(FrameInstallError::HostRealm)?;
        super::history::preflight_navigation(&self.inner, &prepared.final_url, &prepared.request.source, self.inner.replace_navigation.get())
            .map_err(|error| FrameInstallError::ResourceLimit(format!("{error:?}")))?;
        let old_realm = self.inner.realm.borrow().clone();
        let old_proxy_metadata = self.inner.proxy_metadata.borrow().clone();

        *self.inner.proxy_metadata.borrow_mut() = prepared.metadata.clone();
        if ctx.retarget_window_proxy(&proxy, &prepared.realm).is_err() {
            *self.inner.proxy_metadata.borrow_mut() = old_proxy_metadata;
            return Err(FrameInstallError::HostRealm);
        }
        if ctx
            .set_host_global_this(&prepared.realm, proxy.clone())
            .is_err()
        {
            let _ = ctx.retarget_window_proxy(&proxy, &old_realm);
            *self.inner.proxy_metadata.borrow_mut() = old_proxy_metadata;
            return Err(FrameInstallError::HostRealm);
        }

        group.unregister_active(&old_realm);
        // HTML's cross-origin name reset applies only to top-level navigables.
        if self.inner.parent.is_none()
            && !self.inner.root_or_child_origin().same_origin(&metadata_origin(&prepared.metadata))
        {
            self.inner.target_name.borrow_mut().clear();
        }
        *self.inner.realm.borrow_mut() = prepared.realm.clone();
        *self.inner.metadata.borrow_mut() = prepared.metadata.clone();
        *self.inner.proxy_metadata.borrow_mut() = prepared.metadata.clone();
        group.register(&self.inner);
        self.inner
            .inherit_about_origin
            .set(prepared.inherits_creator);
        let old_document = self.inner.document.replace(Some(document.clone()));
        self.inner.javascript_document.borrow_mut().take();
        self.inner.source_bytes.set(prepared.source_bytes);
        self.inner
            .navigation_generation
            .set(self.inner.navigation_generation.get().wrapping_add(1));

        let mut retired_realms = Vec::new();
        if let Some(old_document) = old_document {
            let replace = self.inner.replace_navigation.replace(false) || old_document.lifecycle.initial_about_blank.get();
            super::navigation_lifecycle::destroy(ctx, &old_document);
            old_document.retire_all_frame_contexts(ctx, &mut retired_realms);
            let target = self.inner.history_navigation.borrow_mut().take();
            *self.inner.committed_history_entry.borrow_mut() = target.clone();
            super::history::navigation_finished(ctx, &self.inner, target, Some(&document), prepared.request.source.clone(), replace)
                .map_err(|_| FrameInstallError::HostRealm)?;
        }
        if let Some(staging) = prepared.staging_realm.take() { retired_realms.push(staging); }
        else { retired_realms.push(old_realm); }
        prepared.committed = true;
        super::focus::admit_created_document(&document);
        Ok(FrameCommitResult {
            document,
            retired_realms,
        })
    }
}

fn is_about_blank_url(value: &str) -> bool {
    lumen_common::url::parse(value, None)
        .is_ok_and(|url| url.scheme.eq_ignore_ascii_case("about") && url.path == "blank")
}

fn dispose_compat_realm(ctx: &mut Ctx, realm: &RealmHandle) {
    super::scheduling::cancel_tasks_for_realm(ctx, realm);
    let _ = ctx.dispose_host_realm(realm);
}

fn parse_frame_document(
    source: String,
    content_type: &str,
    final_url: &str,
    max_nodes: usize,
    controller: &Rc<super::dialog_popover::DetailsController>,
) -> Result<(String, lumen_html::Document, bool, Option<String>), FrameInstallError> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .to_ascii_lowercase();
    match essence.as_str() {
        "text/html" => {
            let mut document = lumen_html::Document::new(max_nodes);
            document.set_html_document(true);
            document.set_scripting_enabled(true);
            document.set_allow_declarative_shadow_roots(true);
            controller.attach(&mut document);
            Ok((essence, document, true, Some(source)))
        }
        "application/xhtml+xml" => {
            let mut document = lumen_html::Document::new(max_nodes);
            document.set_scripting_enabled(true);
            controller.attach(&mut document);
            Ok((essence, document, false, Some(source)))
        }
        mime if lumen_common::mime::is_xml_mime(Some(mime)) => {
            let mut document = lumen_html::Document::new(max_nodes);
            document.set_scripting_enabled(true);
            controller.attach(&mut document);
            Ok((essence, document, false, Some(source)))
        }
        mime if lumen_common::mime::is_text_document_mime(Some(mime)) => {
            let source = html::normalize_plaintext(source)
                .map_err(|error| FrameInstallError::Parse(InstallError::Parse(error)))?;
            let mut document = lumen_html::Document::new(max_nodes);
            controller.attach(&mut document);
            document.set_html_document(true);
            let root = document.root();
            let html = create_html_element(&mut document, "html")?;
            let head = create_html_element(&mut document, "head")?;
            let body = create_html_element(&mut document, "body")?;
            let pre = create_html_element(&mut document, "pre")?;
            document
                .append(root, html)
                .map_err(FrameInstallError::Document)?;
            document
                .append(html, head)
                .map_err(FrameInstallError::Document)?;
            document
                .append(html, body)
                .map_err(FrameInstallError::Document)?;
            document
                .append(body, pre)
                .map_err(FrameInstallError::Document)?;
            if !source.is_empty() {
                let text = document
                    .create(lumen_html::NodeKind::Text(source))
                    .map_err(FrameInstallError::Document)?;
                document
                    .append(pre, text)
                    .map_err(FrameInstallError::Document)?;
            }
            Ok((essence, document, true, None))
        }
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/bmp"
        | "image/x-ms-bmp" => {
            let mut document = lumen_html::Document::new(max_nodes);
            controller.attach(&mut document);
            document.set_html_document(true);
            let root = document.root();
            let html = create_html_element(&mut document, "html")?;
            let head = create_html_element(&mut document, "head")?;
            let body = create_html_element(&mut document, "body")?;
            let image = create_html_element(&mut document, "img")?;
            document
                .set_attribute_ns(image, None, "src", final_url)
                .map_err(FrameInstallError::Document)?;
            document
                .append(root, html)
                .map_err(FrameInstallError::Document)?;
            document
                .append(html, head)
                .map_err(FrameInstallError::Document)?;
            document
                .append(html, body)
                .map_err(FrameInstallError::Document)?;
            document
                .append(body, image)
                .map_err(FrameInstallError::Document)?;
            Ok((essence, document, true, None))
        }
        _ => Err(FrameInstallError::UnsupportedContentType(essence)),
    }
}

fn create_html_element(
    document: &mut lumen_html::Document,
    local_name: &str,
) -> Result<NodeId, FrameInstallError> {
    document
        .create(lumen_html::NodeKind::Element {
            namespace: lumen_html::Namespace::Html,
            name: lumen_html::Name::new(local_name),
            attributes: Vec::new(),
        })
        .map_err(FrameInstallError::Document)
}

fn request_for_frame(context: &BrowsingContext) -> FrameNavigationRequest {
    if let Some((source,url))=context.javascript_document.borrow().clone() {
        let document=context.document().expect("active navigable document");
        return FrameNavigationRequest {owner:context.owner_node.unwrap_or_else(||document.session.borrow().document().root()),source:FrameSource::JavaScriptDocument{source,url},base_url:document.base_url(),generation:context.navigation_generation.get(),unsupported:None,post_resource:None,metadata:NavigationMetadata::from_document(&document),initiator_origin:context.root_or_child_origin()};
    }
    if context.parent.is_none() || context.history_navigation.borrow().is_some() || context.committed_history_entry.borrow().is_some() {
        let document = context.document().expect("active navigable has a document");
        let target = context.history_navigation.borrow().clone().or_else(|| context.committed_history_entry.borrow().clone());
        let source = target.as_ref().map(|entry| match &entry.document_state().resource {
            source @ (FrameSource::SrcDoc(_) | FrameSource::JavaScriptDocument {..}) => source.clone(), _ => FrameSource::Url(entry.url()),
        }).unwrap_or_else(||
            FrameSource::Url(context.location_navigation.borrow().clone().unwrap_or_else(|| context.current_document_url())));
        return FrameNavigationRequest { owner: context.owner_node.unwrap_or_else(|| document.session.borrow().document().root()), source,
            base_url: target.as_ref().map_or_else(|| context.navigation_initiator_base.borrow().clone().unwrap_or_else(|| document.base_url()), |entry| entry.document_state().base_url.clone()), generation: context.navigation_generation.get(), unsupported: None,
            post_resource: target.as_ref().and_then(|entry| entry.document_state().post_resource.clone()),
            metadata: context.navigation_metadata.borrow().clone().unwrap_or_else(|| NavigationMetadata::from_document(&document)),
            initiator_origin: context.navigation_initiator.borrow().clone().unwrap_or_else(|| context.root_or_child_origin()) };
    }
    let owner = context.owner_node.expect("frame context has an owner node");
    let owner_realm = context
        .owner_realm
        .as_ref()
        .and_then(std::rc::Weak::upgrade);
    let Some(owner_realm) = owner_realm else {
        return FrameNavigationRequest {
            owner,
            source: FrameSource::Blank,
            base_url: "about:blank".into(),
            generation: context.navigation_generation.get(),
            unsupported: Some(FrameUnsupportedReason::InvalidSourceUrl),
            initiator_origin: context.root_or_child_origin(),
            post_resource: None,
            metadata: NavigationMetadata { policy_container: Default::default(), cookie_source_url: None, referrer: lumen_common::referrer::Referrer { source: "about:blank".into(), policy: Default::default() }, inherited_referrer_policy: Default::default(), source_element: None, user_involvement: UserNavigationInvolvement::None },
        };
    };
    let location_navigation = context.location_navigation.borrow().clone();
    let base_url = context.navigation_initiator_base.borrow().clone()
        .unwrap_or_else(|| owner_realm.base_url());
    let (srcdoc, src, sandbox) = {
        let session = owner_realm.session.borrow();
        let document = session.document();
        let iframe = matches!(document.kind(owner),Ok(NodeKind::Element{namespace:Namespace::Html,name,..}) if name.as_str()=="iframe");
        let srcdoc = iframe.then(|| document
            .get_attribute_ns(owner, None, "srcdoc").ok().flatten()).flatten();
        let src = iframe.then(|| document.get_attribute_ns(owner, None, "src").ok().flatten()).flatten();
        let sandbox = iframe.then(|| document.get_attribute_ns(owner,None,"sandbox").ok().flatten()).flatten();
        (srcdoc, src, sandbox)
    };
    let is_attribute_navigation = location_navigation.is_none() && srcdoc.is_none()
        && !lumen_html::object::is_embedded(owner_realm.session.borrow().document(), owner);
    let mut source = if let Some(url) = location_navigation {
        FrameSource::Url(url)
    } else if let Some(srcdoc) = srcdoc {
        FrameSource::SrcDoc(srcdoc)
    } else if let Some(src) = src.filter(|src| !src.is_empty()) {
        match lumen_common::url::parse(&src, Some(&base_url)) {
            Ok(url) => FrameSource::Url(url.href()),
            Err(_) => FrameSource::Blank,
        }
    } else {
        FrameSource::Blank
    };
    if is_attribute_navigation {
        let candidate = match &source {
            FrameSource::Url(url) => url.as_str(),
            FrameSource::Blank => "about:blank",
            _ => "",
        };
        if let Ok(mut candidate) = lumen_common::url::parse(candidate, None) {
            candidate.fragment = None;
            let mut ancestor = context.parent_context();
            while let Some(current) = ancestor {
                if lumen_common::url::parse(&current.current_document_url(), None).is_ok_and(|mut url| {
                    url.fragment = None;
                    url.href() == candidate.href()
                }) { source = FrameSource::Suppressed; break; }
                ancestor = current.parent_context();
            }
        }
    }
    let sandboxed = sandbox.is_some();
    let unsupported = if sandboxed {
        Some(FrameUnsupportedReason::SandboxPolicy)
    } else if matches!(source, FrameSource::InvalidUrl(_)) {
        Some(FrameUnsupportedReason::InvalidSourceUrl)
    } else {
        None
    };
    FrameNavigationRequest {
        owner,
        source,
        base_url,
        generation: context.navigation_generation.get(),
        unsupported,
        initiator_origin: context.navigation_initiator.borrow().clone()
            .unwrap_or_else(|| owner_realm.document_origin().unwrap_or_else(|| context.root_or_child_origin())),
        post_resource: None,
        metadata: context.navigation_metadata.borrow().clone().unwrap_or_else(|| NavigationMetadata::from_document(&owner_realm)),
    }
}

impl BrowsingContext {
    pub(crate) fn lifecycle_documents(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<Vec<(Rc<DomRealm>, RealmHandle)>> {
        FrameContext { inner: self.clone() }.lifecycle_documents(ctx)
    }
    pub(crate) fn request_history_navigation(&self, entry: Rc<super::history::Entry>) {
        *self.navigation_metadata.borrow_mut() = Some(entry.document_state().metadata.clone());
        *self.history_navigation.borrow_mut() = Some(entry);
        self.navigation_generation.set(self.navigation_generation.get().wrapping_add(1));
        self.beforeunload_generation.set(Some(self.navigation_generation.get()));
    }
    pub(crate) fn history_store(&self) -> Option<Rc<super::history::HistoryStore>> { self.group.upgrade().map(|group| group.history.clone()) }
    pub(crate) fn history_id(&self) -> u64 { self.insertion_order }
    pub(crate) fn next_transition_generation(&self) -> OpResult<u64> {
        let group=self.group.upgrade().ok_or_else(||OpError::new("InvalidStateError","document browsing context no longer available"))?;
        let next=group.transition_generation.get().checked_add(1).ok_or_else(||OpError::new("QuotaExceededError","transition generation exhausted"))?;
        group.transition_generation.set(next);
        Ok(next)
    }
    pub(crate) fn container_node(&self)->Option<NodeId> {self.owner_node}
    pub(crate) fn container_document(&self) -> Option<Rc<DomRealm>> { self.owner_realm.as_ref().and_then(std::rc::Weak::upgrade) }
    pub(crate) fn history_navigation_pending(&self) -> bool { self.history_navigation.borrow().is_some() }
    pub(crate) fn take_post_resource(&self) -> Option<Rc<NavigationPostResource>> { self.pending_post_resource.borrow_mut().take() }
    pub(crate) fn pending_post_resource(&self) -> Option<Rc<NavigationPostResource>> { self.pending_post_resource.borrow().clone() }
    pub(crate) fn replaces_history_entry(&self) -> bool { self.replace_navigation.get() || self.document().is_some_and(|document| document.lifecycle.initial_about_blank.get()) }
    pub(crate) fn allows_modals(&self) -> bool {
        let (Some(owner), Some(node)) = (self.owner_realm.as_ref().and_then(std::rc::Weak::upgrade), self.owner_node) else { return true; };
        let session = owner.session.borrow();
        match session.document().get_attribute_ns_ref(node, None, "sandbox") {
            Ok(Some(flags)) => flags.split_ascii_whitespace().any(|flag| flag.eq_ignore_ascii_case("allow-modals")),
            Ok(None) => true,
            Err(_) => false,
        }
    }
    pub(crate) fn is_active(&self) -> bool {
        if !self.active.get() { return false; }
        // Removing steps logically destroy the navigable synchronously. The
        // existing pending set defers host-realm disposal until native borrows
        // are released; it must not defer author-observable liveness or tasks.
        if let (Some(owner), Some(node)) = (
            self.owner_realm.as_ref().and_then(std::rc::Weak::upgrade), self.owner_node,
        ) {
            if owner.pending_frame_contexts.borrow().get(&node)
                .is_some_and(|pending| std::ptr::eq(self, Rc::as_ptr(pending)))
            { return false; }
        }
        self.parent_context().is_none_or(|parent| parent.is_active())
    }

    pub(crate) fn target_name(&self) -> String {
        self.target_name.borrow().clone()
    }

    pub(crate) fn set_target_name(&self, name: String) {
        if self.active.get() {
            if let Some(history) = self.history.borrow().as_ref() { *history.active.borrow().document_state().target_name.borrow_mut() = name.clone(); }
            *self.target_name.borrow_mut() = name;
        }
    }

    pub(crate) fn restore_history_target_name(&self, name: String) { *self.target_name.borrow_mut() = name; }

    pub(crate) fn is_active_realm(&self, realm: &RealmHandle) -> bool {
        self.is_active() && self.realm.borrow().same_realm(realm)
    }
    fn root(ctx: &mut Ctx) -> Result<Rc<Self>, Value> {
        if let Some(service) = RealmServices::<BrowsingContextService>::current(ctx) {
            if let Some(context) = service.context.upgrade() {
                if context.parent.is_none() {
                    return Ok(context);
                }
            }
        }
        let realm = ctx.current_host_realm();
        let group = group_for_root(ctx, &realm);
        if let Some(context) = group.for_realm(&realm) {
            if context.parent.is_none() {
                register_context_service(ctx, context.clone());
                return Ok(context);
            }
        }
        let metadata = Rc::new(RealmMetadata {
            key: Cell::new(realm.global().object_identity().unwrap_or_default()),
            origin: RefCell::new(Origin::opaque()),
            self_proxy: RefCell::new(None),
            parent_proxy: RefCell::new(None),
            top_proxy: RefCell::new(None),
            parent_metadata: None,
            owner_realm: None,
            owner_node: None,
        });
        let proxy_metadata = Rc::new(RefCell::new(metadata.clone()));
        let context = Rc::new(Self {
            history: RefCell::new(None),
            history_navigation: RefCell::new(None),
            committed_history_entry: RefCell::new(None),
            pending_post_resource: RefCell::new(None),
            replace_navigation: Cell::new(false),
            group: Rc::downgrade(&group),
            metadata: RefCell::new(metadata.clone()),
            proxy_metadata: proxy_metadata.clone(),
            realm: RefCell::new(realm),
            window_proxy: RefCell::new(None),
            document: RefCell::new(None),
            parent: None,
            opener: RefCell::new(None),
            owner_realm: None,
            owner_node: None,
            inherit_about_origin: Cell::new(false),
            location_navigation: RefCell::new(None),
            javascript_document: RefCell::new(None),
            navigation_initiator: RefCell::new(None),
            navigation_initiator_base: RefCell::new(None),
            navigation_metadata: RefCell::new(None),
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
            initial_blank_load_dispatched: Cell::new(false),
            beforeunload_generation: Cell::new(None),
            ignored_attribute_navigation: Cell::new(None),
            insertion_order: 0,
            target_name: RefCell::new(String::new()),
            source_bytes: Cell::new(0),
        });
        context
            .make_window_proxy(ctx)
            .map_err(|error| ctx.make_error("Error", error.to_string()))?;
        group.register(&context);
        let proxy = context.proxy();
        *metadata.self_proxy.borrow_mut() = proxy.as_ref().and_then(|value| ctx.weak_value(value));
        *metadata.parent_proxy.borrow_mut() =
            proxy.as_ref().and_then(|value| ctx.weak_value(value));
        *metadata.top_proxy.borrow_mut() = proxy.as_ref().and_then(|value| ctx.weak_value(value));
        register_context_service(ctx, context.clone());
        Ok(context)
    }

    fn make_window_proxy(&self, ctx: &mut Ctx) -> Result<Value, lumen::embed::WindowProxyError> {
        if let Some(proxy) = self
            .window_proxy
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        {
            return Ok(proxy);
        }
        let proxy = ctx.create_window_proxy(
            &self.realm.borrow(),
            Rc::new(ContextWindowProxyPolicy {
                group: self.group.clone(),
                metadata: self
                    .metadata_registry()
                    .ok_or(lumen::embed::WindowProxyError::UnknownRealm)?,
                target_metadata: self.proxy_metadata.clone(),
            }),
        )?;
        *self.window_proxy.borrow_mut() = ctx.weak_value(&proxy);
        Ok(proxy)
    }

    pub(crate) fn root_or_child_origin(&self) -> Origin {
        self.metadata.borrow().origin.borrow().clone()
    }

    fn set_document_origin_from_url(&self, url: &str) {
        let parsed_about = lumen_common::url::parse(url, None).is_ok_and(|parsed| {
            parsed.scheme == "about" && matches!(parsed.path.as_str(), "blank" | "srcdoc")
        });
        if parsed_about && self.inherit_about_origin.get() {
            return;
        }
        self.inherit_about_origin.set(false);
        self.metadata.borrow().origin.replace(Origin::from_url(url));
    }

    pub(crate) fn invalidate_navigation_request(&self) {
        if self.active.get() {
            self.navigation_generation
                .set(self.navigation_generation.get().wrapping_add(1));
        }
    }

    pub(crate) fn invalidate_owner_navigation_request(&self, owner_base_url: String, metadata: NavigationMetadata) {
        if self.document().is_some_and(|document| document.lifecycle.unload_counter.get() != 0) {
            self.ignored_attribute_navigation.set(Some(self.navigation_generation.get()));
            return;
        }
        self.ignored_attribute_navigation.set(None);
        self.history_navigation.borrow_mut().take();
        self.committed_history_entry.borrow_mut().take();
        self.pending_post_resource.borrow_mut().take();
        self.replace_navigation.set(false);
        self.location_navigation.borrow_mut().take();
        *self.navigation_initiator.borrow_mut() = self.parent_context().map(|parent| parent.root_or_child_origin());
        *self.navigation_initiator_base.borrow_mut() = Some(owner_base_url);
        *self.navigation_metadata.borrow_mut() = Some(metadata);
        self.invalidate_navigation_request();
    }

    pub(crate) fn current_document_url(&self) -> String {
        self.document()
            .and_then(|document| document.document_url())
            .unwrap_or_else(|| "about:blank".to_owned())
    }

    pub(crate) fn request_location_navigation_from(
        &self,
        input: &str,
        entry_base_url: &str,
    ) -> OpResult<()> {
        if self.document().is_some_and(|document| document.lifecycle.unload_counter.get() != 0 || document.lifecycle.destroyed.get()) { return Ok(()); }
        if !self.active.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "browsing context is no longer active",
            ));
        }
        let url = lumen_common::url::parse(input, Some(entry_base_url))
            .map_err(|_| OpError::new("SyntaxError", "invalid Location URL"))?;
        self.history_navigation.borrow_mut().take();
        self.committed_history_entry.borrow_mut().take();
        self.pending_post_resource.borrow_mut().take();
        *self.location_navigation.borrow_mut() = Some(url.href());
        *self.navigation_initiator.borrow_mut() = Some(self.root_or_child_origin());
        *self.navigation_initiator_base.borrow_mut() = Some(entry_base_url.to_owned());
        *self.navigation_metadata.borrow_mut() = self.document().map(|document| NavigationMetadata::from_document(&document));
        self.navigation_generation
            .set(self.navigation_generation.get().wrapping_add(1));
        Ok(())
    }

    pub(crate) fn request_location_navigation_with_caller(&self, ctx: &mut Ctx, input: &str, entry_base_url: &str) -> OpResult<()> {
        self.request_location_navigation_with_handling(ctx, input, entry_base_url, false)
    }

    pub(crate) fn request_location_navigation_with_handling(&self, ctx: &mut Ctx, input: &str, entry_base_url: &str, replace: bool) -> OpResult<()> {
        if self.document().is_some_and(|document| document.lifecycle.unload_counter.get() != 0 || document.lifecycle.destroyed.get()) { return Ok(()); }
        let caller = ctx.invocation_host_realm();
        let origin = metadata_for_realm(ctx, &caller).map(|metadata| metadata_origin(&metadata))
            .ok_or_else(|| OpError::new("InvalidStateError", "Navigation has no initiating document"))?;
        let parsed = lumen_common::url::parse(input, Some(entry_base_url)).map_err(|_| OpError::new("SyntaxError", "invalid Location URL"))?;
        let context = self.history_store().and_then(|store| store.contexts.borrow().get(&self.history_id()).and_then(std::rc::Weak::upgrade));
        if parsed.scheme=="javascript" {
            if let Some(context)=context {
                let source=ctx.with_host_realm(&caller,|ctx|super::window_globals::current_dom_realm(ctx)).map_err(host_realm_error)?;
                if let Some(source)=source {context.queue_javascript_navigation(ctx,&parsed.href(),&source)?;}
            }
            return Ok(());
        }
        self.javascript_document.borrow_mut().take();
        if let Some(context) = context {
            if super::history::fragment_navigation(ctx, &context, &parsed.href(), replace)? { return Ok(()); }
        }
        self.history_navigation.borrow_mut().take();
        self.committed_history_entry.borrow_mut().take();
        self.replace_navigation.set(replace);
        self.request_location_navigation_from(input, entry_base_url)?;
        *self.navigation_initiator.borrow_mut() = Some(origin);
        let metadata = ctx.with_host_realm(&caller, |ctx| super::window_globals::current_dom_realm(ctx))
            .map_err(host_realm_error)?.map(|document| NavigationMetadata::from_document(&document));
        *self.navigation_metadata.borrow_mut() = metadata;
        Ok(())
    }

    pub(crate) fn realm_handle(&self) -> RealmHandle {
        self.realm.borrow().clone()
    }

    pub(crate) fn document(&self) -> Option<Rc<DomRealm>> {
        self.document.borrow().clone()
    }

    pub(crate) fn proxy(&self) -> Option<Value> {
        self.window_proxy
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
    }

    /// WindowProxy indexed properties sort the collected child navigables by
    /// their container's insertion epoch (HTML [[GetOwnProperty]]), even after
    /// an atomic DOM move changes the collection's tree order.
    pub(crate) fn indexed_child_proxy(&self, ctx: &mut Ctx, index: u32) -> OpResult<Option<Value>> {
        let Some(document) = self.document() else { return Ok(None); };
        let mut children = document.frame_contexts(ctx)?;
        children.sort_unstable_by_key(|frame| frame.inner.insertion_order);
        Ok(children.get(index as usize).and_then(FrameContext::window_proxy))
    }

    pub(crate) fn named_child_proxy(&self, ctx: &mut Ctx, name: &str) -> OpResult<Option<Value>> {
        let Some(document) = self.document() else { return Ok(None); };
        Ok(document.named_child_windows(ctx)?.into_iter()
            .find(|(_, candidate, _)| candidate == name).map(|(_, _, proxy)| proxy))
    }

    pub(crate) fn child_count(&self) -> usize { self.connected_iframe_nodes().len() }

    pub(crate) fn parent_context(&self) -> Option<Rc<BrowsingContext>> {
        self.parent.as_ref()?.upgrade()
    }

    pub(crate) fn opener_proxy(&self) -> Option<Value> {
        self.opener.borrow().as_ref().and_then(std::rc::Weak::upgrade).and_then(|context|context.proxy())
    }

    pub(crate) fn disown_opener(&self) {self.opener.borrow_mut().take();}

    pub(crate) fn close_auxiliary(&self,ctx:&mut Ctx) {
        if self.parent.is_some() || self.insertion_order==0 {return;}
        let mut retired=Vec::new();self.retire(ctx,&mut retired);
        if let Some(group)=self.group.upgrade(){group.pending_retired.borrow_mut().extend(retired);}
    }

    fn top_context(self: &Rc<Self>) -> Rc<Self> {
        let mut current = self.clone();
        while let Some(parent) = current.parent_context() {
            current = parent;
        }
        current
    }

    /// Select existing navigables first, admitting a real initial empty
    /// auxiliary document through the same bounded group as iframe documents.
    pub(crate) fn choose_navigation_target(self: &Rc<Self>, ctx: &mut Ctx, name: &str) -> OpResult<Option<Rc<Self>>> {
        if name.is_empty() || name.eq_ignore_ascii_case("_self") { return Ok(Some(self.clone())); }
        if name.eq_ignore_ascii_case("_parent") { return Ok(Some(self.parent_context().unwrap_or_else(|| self.clone()))); }
        if name.eq_ignore_ascii_case("_top") { return Ok(Some(self.top_context())); }
        if name.eq_ignore_ascii_case("_blank") {
            if !self.auxiliary_creation_allowed(){return Ok(None);}
            return self.document().map(|document|document.create_auxiliary_context(ctx,String::new())).transpose();
        }
        for root in [self.clone(), self.top_context()] {
            let mut pending = vec![root];
            while let Some(candidate) = pending.pop() {
                if candidate.target_name() == name { return Ok(Some(candidate)); }
                if let Some(document) = candidate.document() {
                    let children = document.frame_contexts(ctx)?;
                    pending.extend(children.into_iter().rev().map(|frame| frame.inner));
                }
            }
        }
        if let Some(group)=self.group.upgrade() {
            let target=group.contexts.borrow().values().find(|context|context.is_active() && context.parent.is_none() && context.target_name()==name).cloned();
            if target.is_some(){return Ok(target);}
        }
        if !self.auxiliary_creation_allowed(){return Ok(None);}
        self.document().map(|document|document.create_auxiliary_context(ctx,name.to_owned())).transpose()
    }

    fn auxiliary_creation_allowed(&self)->bool {
        let (Some(owner),Some(node))=(self.owner_realm.as_ref().and_then(std::rc::Weak::upgrade),self.owner_node) else{return true};
        let session=owner.session.borrow();
        session.document().get_attribute_ns_ref(node,None,"sandbox").ok().flatten().is_none_or(|flags|flags.split_ascii_whitespace().any(|flag|flag.eq_ignore_ascii_case("allow-popups")))
    }

    pub(crate) fn request_hyperlink_navigation(self: &Rc<Self>, ctx: &mut Ctx, url: &str, source: &Rc<DomRealm>, metadata: NavigationMetadata) -> OpResult<()> {
        let Some(source_context) = source.browsing_context().filter(|context| is_active_document(context, source) && context.group.ptr_eq(&self.group)) else { return Ok(()); };
        if lumen_common::url::parse(url,Some(&source.base_url())).is_ok_and(|url|url.scheme=="javascript") {return self.queue_javascript_navigation(ctx,url,source);}
        self.javascript_document.borrow_mut().take();
        if super::history::fragment_navigation(ctx, self, url, false)? { return Ok(()); }
        self.replace_navigation.set(false);
        self.request_location_navigation_from(url, &source.base_url())?;
        *self.navigation_initiator.borrow_mut() = Some(source_context.root_or_child_origin());
        *self.navigation_metadata.borrow_mut() = Some(metadata);
        Ok(())
    }

    pub(crate) fn captured_navigation_metadata(&self) -> NavigationMetadata {
        self.navigation_metadata.borrow().clone().unwrap_or_else(|| {
            NavigationMetadata::from_document(&self.document().expect("active context document"))
        })
    }

    fn queue_javascript_navigation(self:&Rc<Self>,ctx:&mut Ctx,url:&str,source:&Rc<DomRealm>)->OpResult<()> {
        if !self.is_active() || !source.browsing_context().is_some_and(|context|is_active_document(&context,source) && context.same_origin_with(self)) {return Ok(());}
        let parsed=lumen_common::url::parse(url,Some(&source.base_url())).map_err(|_|OpError::new("SyntaxError","invalid javascript URL"))?;
        let href=parsed.href();
        let target_document=self.document().ok_or_else(||OpError::new("InvalidStateError","navigation target has no document"))?;
        if !Rc::ptr_eq(source,&target_document) && !source.prepare_navigation_script_csp(ctx,&href)? {return Ok(());}
        let encoded=href.split_once(':').map(|(_,source)|source).unwrap_or("").split('#').next().unwrap_or("");
        let script=String::from_utf8_lossy(&lumen_common::codec::percent_decode(encoded.as_bytes())).into_owned();
        let context=Rc::downgrade(self);
        let document=Rc::downgrade(&target_document);
        let generation=self.navigation_generation.get();
        let realm=self.realm_handle();
        ctx.with_host_realm(&realm,|ctx|scheduling::queue_task(ctx,move|ctx| {
            let (Some(context),Some(document))=(context.upgrade(),document.upgrade()) else{return Ok(())};
            if !is_active_document(&context,&document) || context.navigation_generation.get()!=generation {return Ok(());}
            if !document.prepare_navigation_script_csp(ctx,&href)? {return Ok(());}
            let completion=match ctx.eval_value_in_host_realm_named(&context.realm_handle(),&script,false,Some(&href)) {
                Ok(Ok(value))=>value,
                Ok(Err(exception))=>return Err(OpError::thrown(exception)),
                Err(lumen::embed::HostRealmEvalError::Parse(error))=>return Err(OpError::new("SyntaxError",error.message)),
                Err(lumen::embed::HostRealmEvalError::Scope(error))=>return Err(host_realm_error(error)),
            };
            if let Value::Str(html)=completion {
                if !is_active_document(&context,&document) || context.navigation_generation.get()!=generation {return Ok(());}
                let limit=context.group.upgrade().map(|group|group.limits.get().max_document_source_bytes).unwrap_or(0);
                if html.as_str().len()>limit {return Err(OpError::new("QuotaExceededError","javascript document source budget exhausted"));}
                *context.javascript_document.borrow_mut()=Some((html.as_str().to_owned(),context.current_document_url()));
                context.replace_navigation.set(true);
                context.invalidate_navigation_request();
            }
            Ok(())
        })).map_err(host_realm_error)??;
        Ok(())
    }

    fn connected_iframe_nodes(&self) -> Vec<NodeId> {
        self.document()
            .map(|realm| realm.connected_iframe_nodes())
            .unwrap_or_default()
    }

    fn same_origin_with(&self, other: &Self) -> bool {
        self.metadata
            .borrow()
            .origin
            .borrow()
            .same_origin(&other.metadata.borrow().origin.borrow())
    }

    fn retire(&self, ctx: &mut Ctx, retired: &mut Vec<RealmHandle>) {
        if !self.active.replace(false) {
            return;
        }
        if let Err(error) = super::history::child_removed(ctx, self) {
            let exception = error.to_value(ctx);
            super::error_reporting::report_exception(ctx, exception);
        }
        self.navigation_generation
            .set(self.navigation_generation.get().wrapping_add(1));
        let retired_document=self.document.borrow_mut().take();
        if let Some(document) = retired_document {
            let realm=self.realm_handle();
            match ctx.with_host_realm(&realm,|ctx|super::animations::retire_document(ctx,&document)) {
                Ok(Ok(()))=>{},
                Ok(Err(error))=>{let exception=error.to_value(ctx);super::error_reporting::report_exception(ctx,exception);},
                Err(error)=>{let exception=OpError::new("InvalidStateError",error.to_string()).to_value(ctx);super::error_reporting::report_exception(ctx,exception);},
            }
            super::navigation_lifecycle::destroy(ctx, &document);
            document.retire_all_frame_contexts(ctx, retired);
        }
        let realm = self.realm_handle();
        ctx.cancel_async_module_imports_for_realm(&realm);
        // Unlike replacement of a Document in a reused Window, destroying
        // this navigable retires the entire realm, including owners registered
        // by parser preparation before document identity was available.
        super::scheduling::cancel_tasks_for_realm(ctx,&realm);
        if let Some(timers) = ctx.host_mut::<lumen_timers::Timers>() { timers.cancel_realm(&realm); }
        if let Some(group) = self.group.upgrade() {
            group.unregister_active(&realm);
        }
        retired.push(realm);
    }

    fn new_realm_metadata(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        realm: &RealmHandle,
        origin: Origin,
    ) -> Rc<RealmMetadata> {
        let parent_proxy = self.parent_context().and_then(|parent| parent.proxy()).or_else(||self.proxy());
        let top_proxy = self.top_context().proxy();
        let metadata = Rc::new(RealmMetadata {
            key: Cell::new(realm.global().object_identity().unwrap_or_default()),
            origin: RefCell::new(origin),
            self_proxy: RefCell::new(
                self.proxy()
                    .as_ref()
                    .and_then(|value| ctx.weak_value(value)),
            ),
            parent_proxy: RefCell::new(
                parent_proxy
                    .as_ref()
                    .and_then(|value| ctx.weak_value(value)),
            ),
            top_proxy: RefCell::new(top_proxy.as_ref().and_then(|value| ctx.weak_value(value))),
            parent_metadata: self
                .parent_context()
                .map(|parent| Rc::downgrade(&parent.metadata.borrow())),
            owner_realm: self.owner_realm.clone(),
            owner_node: self.owner_node,
        });
        if let Some(group) = self.group.upgrade() {
            group.metadata.register(&metadata);
        }
        metadata
    }

    fn metadata_registry(&self) -> Option<Rc<RealmMetadataRegistry>> {
        self.group.upgrade().map(|group| group.metadata.clone())
    }
}

struct ContextWindowProxyPolicy {
    group: std::rc::Weak<ContextGroup>,
    metadata: Rc<RealmMetadataRegistry>,
    target_metadata: Rc<RefCell<Rc<RealmMetadata>>>,
}

impl ContextWindowProxyPolicy {
    fn metadata_for(&self, realm: &RealmHandle) -> Option<Rc<RealmMetadata>> {
        self.metadata.for_realm(realm)
    }

    fn target_metadata(&self, realm: &RealmHandle) -> Option<Rc<RealmMetadata>> {
        let current = self.target_metadata.borrow().clone();
        (current.key.get() == realm.global().object_identity()?).then_some(current)
    }

    fn denied(ctx: &mut Ctx) -> Value {
        crate::error_reporting::dom_exception(
            ctx,
            "SecurityError",
            "Permission denied to access a cross-origin browsing context",
        )
        .to_value(ctx)
    }
}

impl WindowProxyPolicy for ContextWindowProxyPolicy {
    fn decide(
        &self,
        ctx: &mut Ctx,
        caller: &RealmHandle,
        target: &RealmHandle,
        operation: &WindowProxyOperation,
    ) -> WindowProxyDisposition {
        let (Some(caller_metadata), Some(target_metadata)) =
            (self.metadata_for(caller), self.target_metadata(target))
        else {
            return WindowProxyDisposition::Denied(Self::denied(ctx));
        };
        if caller_metadata.origin.borrow().same_origin(&target_metadata.origin.borrow()) {
            WindowProxyDisposition::ForwardSameOrigin
        } else {
            let context = self.group.upgrade().and_then(|group| group.for_realm(target));
            match context {
                Some(context) => match window_messaging::cross_origin_operation(ctx, &context, operation) {
                    Ok(result) => WindowProxyDisposition::Handled(result),
                    Err(error) => WindowProxyDisposition::Denied(error.to_value(ctx)),
                },
                None => WindowProxyDisposition::Denied(Self::denied(ctx)),
            }
        }
    }

    fn authorize_native_window_receiver(
        &self,
        ctx: &mut Ctx,
        caller: &RealmHandle,
        target: &RealmHandle,
    ) -> Result<(), Value> {
        let (Some(caller), Some(target)) =
            (self.metadata_for(caller), self.target_metadata(target))
        else {
            return Err(Self::denied(ctx));
        };
        if caller.origin.borrow().same_origin(&target.origin.borrow()) {
            Ok(())
        } else {
            Err(Self::denied(ctx))
        }
    }

    fn child_window_count(
        &self,
        _ctx: &mut Ctx,
        _caller: &RealmHandle,
        target: &RealmHandle,
    ) -> usize {
        self.group
            .upgrade()
            .and_then(|group| group.for_realm(target))
            .map(|context| context.connected_iframe_nodes().len())
            .unwrap_or(0)
    }

    fn child_window_at(
        &self,
        ctx: &mut Ctx,
        _caller: &RealmHandle,
        target: &RealmHandle,
        index: u32,
    ) -> Option<Value> {
        let group = self.group.upgrade()?;
        let target_context = group.for_realm(target)?;
        let target_handle = target_context.realm_handle();
        ctx.with_host_realm(&target_handle, |ctx| {
            target_context.indexed_child_proxy(ctx, index).ok().flatten()
        })
        .ok()
        .flatten()
    }
}

impl DomRealm {
    /// The active document's navigable, including a host-controlled top level.
    pub fn navigation_context(&self) -> Option<FrameContext> { self.browsing_context().map(|inner| FrameContext { inner }) }
    /// Deduplicate child target names in document tree order before filtering
    /// origins. A foreign first child hides a later same-origin namesake.
    pub(crate) fn named_child_windows(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<Vec<(NodeId, String, Value)>> {
        if !self.browsing_context().is_some_and(|context| is_active_document(&context, self)) {
            return Ok(Vec::new());
        }
        let origin = self.document_origin().or_else(|| self.browsing_context().map(|context| context.root_or_child_origin()));
        let mut seen = HashSet::new();
        let mut windows = Vec::new();
        for frame in self.frame_contexts(ctx)? {
            let name = frame.inner.target_name();
            if name.is_empty() || !seen.insert(name.clone()) { continue; }
            if !origin.as_ref().is_some_and(|origin| origin.same_origin(&frame.origin())) { continue; }
            if let Some(proxy) = frame.window_proxy() { windows.push((frame.owner_node(), name, proxy)); }
        }
        Ok(windows)
    }

    /// Once another named object contributes a supported name, navigables have
    /// value priority even when their origin excluded that name from the set.
    pub(crate) fn named_child_value(self: &Rc<Self>, ctx: &mut Ctx, name: &str) -> OpResult<Option<Value>> {
        if !self.browsing_context().is_some_and(|context| is_active_document(&context, self)) {
            return Ok(None);
        }
        Ok(self.frame_contexts(ctx)?.into_iter().find(|frame| frame.target_name() == name)
            .and_then(|frame| frame.window_proxy()))
    }

    /// Configure aggregate iframe admission before author code can create child
    /// contexts. Every synchronous insertion and staged navigation consults it.
    pub fn set_frame_resource_limits(&self, limits: FrameResourceLimits) -> OpResult<()> {
        if limits.max_realms == 0 || limits.max_document_nodes == 0 || limits.max_total_nodes == 0
            || limits.max_document_source_bytes > limits.max_total_source_bytes {
            return Err(OpError::type_error("Invalid frame resource limits"));
        }
        let group = self.browsing_context().and_then(|context| context.group.upgrade())
            .ok_or_else(|| OpError::new("InvalidStateError", "Document has no browsing-context group"))?;
        group.limits.set(limits);
        Ok(())
    }

    pub(crate) fn render_capture_pixel_budget(&self) -> OpResult<std::sync::Arc<lumen_common::limits::ByteBudget>> {
        let context = self.browsing_context()
            .ok_or_else(|| OpError::new("InvalidStateError", "Render capture document has no browsing context"))?;
        FrameContext { inner: context }.embedded_pixel_budget()
    }

    pub(crate) fn browsing_context(&self) -> Option<Rc<BrowsingContext>> {
        self.browsing_context.borrow().upgrade()
    }

    pub(crate) fn connected_iframe_nodes(&self) -> Vec<NodeId> {
        let session = self.session.borrow();
        let document = session.document();
        let root = document.root();
        let mut out = Vec::new();
        let mut next = selector::next_shadow_including_descendant(document, root, root)
            .ok()
            .flatten();
        while let Some(node) = next {
            if script_loading::is_connected(document, node)
                && matches!(
                    document.kind(node),
                    Ok(NodeKind::Element {
                        namespace: Namespace::Html,
                        name,
                        ..
                    }) if lumen_html::xml::split_qname(name.as_str())
                        .is_some_and(|(_, local_name)| local_name == "iframe" || matches!(local_name, "object" | "embed")
                            && self.object_resources.representation(node).has_child_navigable())
                )
            {
                out.push(node);
            }
            next = selector::next_shadow_including_descendant(document, root, node)
                .ok()
                .flatten();
        }
        out
    }

    /// Existing active contexts only: rendering never creates navigables or requests.
    pub(crate) fn embedded_paint_contexts(&self)->OpResult<Vec<(NodeId,FrameContext,Rc<DomRealm>)>> {
        let contexts=self.frame_contexts.borrow();
        let mut out=Vec::new();
        out.try_reserve(contexts.len()).map_err(|_|OpError::new("QuotaExceededError","Embedded paint metadata exceeds available memory"))?;
        let session=self.session.borrow();
        for (node,weak) in contexts.iter() {
            if !script_loading::is_connected(session.document(),*node) {continue;}
            if lumen_html::object::is_embedded(session.document(),*node)
                && !self.object_resources.representation(*node).has_child_navigable() {continue;}
            let Some(context)=weak.upgrade().filter(|context|context.is_active()) else {continue;};
            let Some(document)=context.document.borrow().clone().filter(|document|is_active_document(&context,document)) else {continue;};
            out.push((*node,FrameContext{inner:context},document));
        }
        out.sort_unstable_by_key(|(node,_,_)|node.key());
        Ok(out)
    }

    /// Only actual active embedded-document representations expose natural size
    /// metadata to their owner; iframe dimensions never follow child content.
    pub(crate) fn embedded_document(&self,node:NodeId)->Option<Rc<DomRealm>> {
        if !lumen_html::object::is_embedded(self.session.borrow().document(),node)
            || !self.object_resources.representation(node).has_child_navigable() { return None; }
        let context=self.frame_contexts.borrow().get(&node)?.upgrade()?;
        if !context.active.get() { return None; }
        let document=context.document.borrow().clone()?;
        is_active_document(&context,&document).then_some(document)
    }

    pub(crate) fn ensure_frame_context(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
    ) -> OpResult<FrameContext> {
        let connected = {
            let session = self.session.borrow();
            script_loading::is_connected(session.document(), node)
                || lumen_html::object::kind(session.document(),node)==Some(lumen_html::object::Kind::Embed) && self.object_resources.embed_was_connected(node)
        };
        if !self.has_browsing_context || !connected {
            return Err(OpError::type_error("iframe has no active browsing context"));
        }
        self.retire_pending_frame_context_for_node(ctx, node);
        if let Some(context) = self
            .frame_contexts
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
            .filter(|context| context.active.get())
        {
            return Ok(FrameContext { inner: context });
        }
        let parent = self
            .browsing_context()
            .ok_or_else(|| OpError::type_error("document has no browsing context"))?;
        let (sandboxed, sandbox_allows_same_origin) = {
            let session = self.session.borrow();
            let document = session.document();
            let sandbox = if lumen_html::object::is_embedded(document,node) { None } else {
                document.get_attribute_ns(node,None,"sandbox").map_err(super::dom_error)?
            };
            let allow_same_origin = sandbox.as_deref().is_some_and(|value| {
                super::html_space_tokens(value)
                    .any(|token| token.eq_ignore_ascii_case("allow-same-origin"))
            });
            (sandbox.is_some(), allow_same_origin)
        };
        let name=self.session.borrow().document().get_attribute_ns(node,None,"name").map_err(super::dom_error)?.unwrap_or_default();
        self.create_initial_context(ctx,parent,Some(node),sandboxed,sandbox_allows_same_origin,name)
    }

    fn create_initial_context(self:&Rc<Self>,ctx:&mut Ctx,parent:Rc<BrowsingContext>,node:Option<NodeId>,sandboxed:bool,sandbox_allows_same_origin:bool,target_name:String)->OpResult<FrameContext> {
        // Creating the navigable always publishes its initial empty document.
        // Attribute processing starts src/srcdoc navigation separately.
        let source = String::new();
        let url = "about:blank";
        let origin = if self.lifecycle.sandboxed_origin.get() || (sandboxed && !sandbox_allows_same_origin) {
            Origin::opaque()
        } else {
            Origin::for_document(url, Some(&parent.root_or_child_origin()))
        };
        let group = parent
            .group
            .upgrade()
            .ok_or_else(|| OpError::type_error("browsing context group has retired"))?;
        let (mut reservation, max_nodes) = group.admit(ctx, source.len())?;
        let insertion_order = group.next_insertion.get();
        let next_insertion = insertion_order.checked_add(1)
            .ok_or_else(|| OpError::new("QuotaExceededError", "Frame insertion sequence exhausted"))?;
        group.next_insertion.set(next_insertion);
        let child_realm = ctx.create_host_realm();
        let parsed = ctx.with_host_realm(&child_realm, |ctx| {
            let controller = super::dialog_popover::DetailsController::prepare(ctx)?;
            let document = html::parse_with_options_initialized(&source, max_nodes,
                html::ParseOptions { allow_declarative_shadow_roots: true, ..Default::default() },
                |document| controller.attach(document))
                .map_err(|error| OpError::error(format!("{error:?}")))?;
            Ok::<_, OpError>((document, controller))
        });
        let (document, details_controller) = match parsed {
            Ok(Ok(parsed)) => parsed,
            Ok(Err(error)) => {
                dispose_compat_realm(ctx, &child_realm);
                return Err(error);
            }
            Err(_) => {
                dispose_compat_realm(ctx, &child_realm);
                return Err(OpError::error("failed to parse iframe initial document"));
            }
        };
        reservation.set_nodes(document.node_count());
        let parent_proxy = parent.proxy();
        let top_proxy = parent.top_context().proxy();
        let metadata = Rc::new(RealmMetadata {
            key: Cell::new(child_realm.global().object_identity().unwrap_or_default()),
            origin: RefCell::new(origin),
            self_proxy: RefCell::new(None),
            parent_proxy: RefCell::new(
                parent_proxy
                    .as_ref()
                    .and_then(|value| ctx.weak_value(value)),
            ),
            top_proxy: RefCell::new(top_proxy.as_ref().and_then(|value| ctx.weak_value(value))),
            parent_metadata: node.map(|_|Rc::downgrade(&parent.metadata.borrow())),
            owner_realm: node.map(|_|Rc::downgrade(self)),
            owner_node: node,
        });
        let proxy_metadata = Rc::new(RefCell::new(metadata.clone()));
        let child = Rc::new(BrowsingContext {
            history: RefCell::new(None),
            history_navigation: RefCell::new(None),
            committed_history_entry: RefCell::new(None),
            pending_post_resource: RefCell::new(None),
            replace_navigation: Cell::new(false),
            group: Rc::downgrade(&group),
            metadata: RefCell::new(metadata.clone()),
            proxy_metadata: proxy_metadata.clone(),
            realm: RefCell::new(child_realm.clone()),
            window_proxy: RefCell::new(None),
            document: RefCell::new(None),
            parent: node.map(|_|Rc::downgrade(&parent)),
            opener: RefCell::new(node.is_none().then(||Rc::downgrade(&parent))),
            owner_realm: node.map(|_|Rc::downgrade(self)),
            owner_node: node,
            inherit_about_origin: Cell::new(true),
            location_navigation: RefCell::new(None),
            javascript_document: RefCell::new(None),
            navigation_initiator: RefCell::new(Some(parent.root_or_child_origin())),
            navigation_initiator_base: RefCell::new(Some(self.base_url())),
            navigation_metadata: RefCell::new(Some(node.map_or_else(||NavigationMetadata::from_document(self),|node|NavigationMetadata::from_frame_owner(self,self.session.borrow().document(),node)))),
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
            initial_blank_load_dispatched: Cell::new(false),
            beforeunload_generation: Cell::new(None),
            ignored_attribute_navigation: Cell::new(None),
            insertion_order,
            target_name: RefCell::new(target_name),
            source_bytes: Cell::new(source.len()),
        });
        let proxy = match child.make_window_proxy(ctx) {
            Ok(proxy) => proxy,
            Err(_) => {
                dispose_compat_realm(ctx, &child_realm);
                return Err(OpError::error("failed to create iframe WindowProxy"));
            }
        };
        *metadata.self_proxy.borrow_mut() = ctx.weak_value(&proxy);
        if node.is_none() {
            *metadata.parent_proxy.borrow_mut()=ctx.weak_value(&proxy);
            *metadata.top_proxy.borrow_mut()=ctx.weak_value(&proxy);
        }
        group.register(&child);
        let inherited_base = self.base_url();
        let install = ctx.with_host_realm(&child_realm, |ctx| {
            register_context_service(ctx, child.clone());
            ctx.set_host_global_this(&child_realm, proxy.clone())
                .map_err(|_| OpError::error("failed to publish iframe WindowProxy"))?;
            let global = ctx.global_object();
            for name in ["window", "self"] {
                ctx.set_member(&global, name, proxy.clone())
                    .map_err(|_| OpError::error("failed to initialize iframe global"))?;
            }
            lumen_host::install_registered_host_realm(ctx, &child_realm).map_err(OpError::error)?;
            super::install_document_with_context_metadata(
                ctx,
                document,
                "text/html",
                true,
                true,
                Some(child.clone()),
                Some(url.to_owned()),
                Some(inherited_base),
                None,
                true,
                Some(details_controller),
                false,
            )
            .map_err(|error| {
                OpError::error(format!(
                    "failed to install iframe initial document: {error:?}"
                ))
            })
        });
        match install {
            Ok(Ok(realm)) => {
                realm.initialize_permissions_policy(self,node)?;
                if let Some(node)=node {realm.inherit_embedding_color_scheme(self,node)?;}
                realm.inherit_policy_container(&self.policy_container());
                realm.inherit_cookie_environment(self);
                realm.referrer_policy.set(self.referrer_policy.get());
                *realm.document_referrer.borrow_mut() = self.document_url().unwrap_or_else(|| "about:blank".into());
                realm.lifecycle.initial_about_blank.set(true);
                let automatic_features_blocked=self.lifecycle.sandboxed_automatic_features.get()
                    || node.is_some_and(|node|!lumen_html::object::is_embedded(self.session.borrow().document(),node) && self.session.borrow().document().get_attribute_ns_ref(node,None,"sandbox").ok().flatten()
                        .is_some_and(|flags|!flags.split_ascii_whitespace().any(|flag|flag.eq_ignore_ascii_case("allow-scripts"))));
                realm.lifecycle.sandboxed_automatic_features.set(automatic_features_blocked);
                ctx.with_host_realm(&child_realm, |ctx| realm.set_document_ready_state(ctx, super::DocumentReadyState::Complete))
                    .map_err(|_| OpError::error("failed to initialize iframe readiness"))??;
                group.track_document(&realm, source.len());
                *child.document.borrow_mut() = Some(realm);
                drop(reservation);
                if let Some(node)=node {self.frame_contexts.borrow_mut().insert(node,Rc::downgrade(&child));}
                let frame = FrameContext {
                    inner: child.clone(),
                };
                let _ = proxy;
                Ok(frame)
            }
            Ok(Err(error)) => {
                RealmServices::<BrowsingContextService>::remove_for_global(
                    ctx,
                    &child_realm.global(),
                );
                group.unregister_active(&child_realm);
                dispose_compat_realm(ctx, &child_realm);
                Err(error)
            }
            Err(_) => {
                RealmServices::<BrowsingContextService>::remove_for_global(
                    ctx,
                    &child_realm.global(),
                );
                group.unregister_active(&child_realm);
                dispose_compat_realm(ctx, &child_realm);
                Err(OpError::error("failed to install iframe initial document"))
            }
        }
    }

    /// Materialize connected iframe children in tree order for an embedder's
    /// navigation and load-event pump. Detached child realms remain queued for
    /// explicit host cleanup; this method never silently disposes their work.
    pub fn frame_contexts(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<Vec<FrameContext>> {
        let nodes = self.connected_iframe_nodes();
        nodes
            .into_iter()
            .map(|node| self.ensure_frame_context(ctx, node))
            .collect()
    }

    pub fn auxiliary_contexts(&self) -> Vec<FrameContext> {
        let Some(group)=self.browsing_context().and_then(|context|context.group.upgrade()) else{return Vec::new()};
        let mut contexts=group.contexts.borrow().values().filter(|context|context.is_active() && context.parent.is_none() && context.insertion_order!=0)
            .cloned().collect::<Vec<_>>();
        contexts.sort_unstable_by_key(|context|context.insertion_order);
        contexts.into_iter().map(|inner|FrameContext{inner}).collect()
    }

    pub(crate) fn create_auxiliary_context(self:&Rc<Self>,ctx:&mut Ctx,name:String)->OpResult<Rc<BrowsingContext>> {
        let parent=self.browsing_context().filter(|context|is_active_document(context,self)).ok_or_else(||OpError::type_error("Document has no active browsing context"))?;
        Ok(self.create_initial_context(ctx,parent,None,false,true,name)?.inner)
    }

    /// Release host ownership of iframe navigables whose owner element has been detached.
    /// Compatibility helper for embedders without Runtime-owned tasks. Production
    /// hosts should use `take_detached_frame_realms` and cancel their own work first.
    pub fn retire_detached_frame_contexts(&self, ctx: &mut Ctx) -> usize {
        let (count, realms) = self.collect_detached_frame_realms(ctx);
        for realm in realms {
            dispose_compat_realm(ctx, &realm);
        }
        count
    }

    /// Remove detached or synchronously removed-and-reinserted child navigables
    /// and return their realms for host task cancellation and disposal. The host
    /// should call this before `frame_contexts` in its lifecycle pump.
    pub fn take_detached_frame_realms(&self, ctx: &mut Ctx) -> Vec<RealmHandle> {
        let mut retired=self.collect_detached_frame_realms(ctx).1;
        if let Some(group)=self.browsing_context().and_then(|context|context.group.upgrade()) {retired.extend(core::mem::take(&mut *group.pending_retired.borrow_mut()));}
        retired
    }

    fn collect_detached_frame_realms(&self, ctx: &mut Ctx) -> (usize, Vec<RealmHandle>) {
        let connected = self.connected_iframe_nodes();
        let mut retired_contexts = std::mem::take(&mut *self.pending_frame_contexts.borrow_mut());
        {
            let mut cache = self.frame_contexts.borrow_mut();
            cache.retain(|node, weak| {
                let Some(context) = weak.upgrade() else {
                    return false;
                };
                if connected.contains(node)
                    && context.active.get()
                    && !retired_contexts.contains_key(node)
                {
                    return true;
                }
                retired_contexts.entry(*node).or_insert(context);
                false
            });
        }

        let count = retired_contexts
            .values()
            .filter(|context| context.active.get())
            .count();
        let mut realm_handles = std::mem::take(&mut *self.pending_frame_realms.borrow_mut());
        for context in retired_contexts.into_values() {
            context.retire(ctx, &mut realm_handles);
        }
        (count, realm_handles)
    }

    pub(crate) fn represented_object_context(&self, node: NodeId) -> Option<FrameContext> {
        if !self.object_resources.representation(node).has_child_navigable() { return None; }
        self.frame_contexts.borrow().get(&node).and_then(std::rc::Weak::upgrade)
            .filter(|context| context.active.get()).map(|inner| FrameContext { inner })
    }

    pub(crate) fn destroy_object_context(&self, ctx: &mut Ctx, node: NodeId) {
        let context = self.frame_contexts.borrow_mut().remove(&node).and_then(|context| context.upgrade());
        let pending = self.pending_frame_contexts.borrow_mut().remove(&node);
        let mut retired = Vec::new();
        if let Some(context) = context.as_ref() { context.retire(ctx, &mut retired); }
        if let Some(pending) = pending.filter(|pending| context.as_ref().is_none_or(|context| !Rc::ptr_eq(context, pending))) {
            pending.retire(ctx, &mut retired);
        }
        self.pending_frame_realms.borrow_mut().extend(retired);
    }

    fn retire_pending_frame_context_for_node(&self, ctx: &mut Ctx, node: NodeId) {
        let context = self.pending_frame_contexts.borrow_mut().remove(&node);
        let Some(context) = context else {
            return;
        };
        {
            let mut contexts = self.frame_contexts.borrow_mut();
            let is_same = contexts
                .get(&node)
                .and_then(std::rc::Weak::upgrade)
                .is_some_and(|current| Rc::ptr_eq(&current, &context));
            if is_same {
                contexts.remove(&node);
            }
        }
        let mut retired = Vec::new();
        context.retire(ctx, &mut retired);
        self.pending_frame_realms.borrow_mut().extend(retired);
    }

    pub(crate) fn retire_all_frame_contexts(&self, ctx: &mut Ctx, retired: &mut Vec<RealmHandle>) {
        let contexts: Vec<Rc<BrowsingContext>> = self
            .frame_contexts
            .borrow_mut()
            .drain()
            .filter_map(|(_, weak)| weak.upgrade())
            .collect();
        for context in contexts {
            context.retire(ctx, retired);
        }
        let pending: Vec<_> = std::mem::take(&mut *self.pending_frame_contexts.borrow_mut())
            .into_values()
            .collect();
        for context in pending {
            context.retire(ctx, retired);
        }
        retired.extend(std::mem::take(&mut *self.pending_frame_realms.borrow_mut()));
    }

    /// Retire this top-level context and its child navigables. The host must call
    /// `Ctx::dispose_host_realm` for returned handles after leaving their active realms.
    pub fn retire_browsing_context_group(&self, ctx: &mut Ctx) -> Vec<RealmHandle> {
        let Some(root) = self.browsing_context() else {
            return Vec::new();
        };
        if root.parent.is_some() {
            return Vec::new();
        }
        let Some(group) = root.group.upgrade() else {
            return Vec::new();
        };
        let contexts: Vec<Rc<BrowsingContext>> =
            group.contexts.borrow().values().cloned().collect();
        let mut retired = Vec::new();
        for context in contexts {
            context.retire(ctx, &mut retired);
        }
        remove_group(ctx, group.root_key);
        retired
    }
}

pub(crate) fn root_context(ctx: &mut Ctx) -> Result<Rc<BrowsingContext>, Value> {
    BrowsingContext::root(ctx)
}

fn context_service(context: Rc<BrowsingContext>) -> BrowsingContextService {
    BrowsingContextService {
        context: Rc::downgrade(&context),
        metadata: context.metadata.borrow().clone(),
        registry: context
            .metadata_registry()
            .expect("active browsing context group"),
    }
}

pub(crate) fn context_proxy(ctx: &mut Ctx, context: &BrowsingContext) -> Option<Value> {
    context.make_window_proxy(ctx).ok()
}

pub(crate) fn context_top(context: &Rc<BrowsingContext>) -> Rc<BrowsingContext> {
    context.top_context()
}

/// Style queries can run before the embedder's next frame pump. Refresh a
/// child document's media viewport from its live iframe content box then.
pub(crate) fn synchronize_frame_media_environment(realm: &DomRealm) -> OpResult<()> {
    let Some(context) = realm.browsing_context() else { return Ok(()); };
    let Some((_, owner, _)) = context_frame_element(&context) else { return Ok(()); };
    if owner.layout_flusher.borrow().is_none() && owner.session.borrow().viewport_size().is_none() {
        return Ok(());
    }
    let (width, height) = FrameContext { inner: context }.content_viewport_size()?;
    let mut session = realm.session.borrow_mut();
    let mut environment = session.media_environment();
    if environment.width == width as f32 && environment.height == height as f32 {
        return Ok(());
    }
    environment.width = width as f32;
    environment.height = height as f32;
    session.set_media_environment(environment)
        .map_err(|error| OpError::new("InvalidStateError", format!("iframe media environment failed: {error:?}")))
}

pub(crate) fn context_frame_element(
    context: &BrowsingContext,
) -> Option<(Rc<BrowsingContext>, Rc<DomRealm>, NodeId)> {
    Some((
        context.parent_context()?,
        context.owner_realm.as_ref()?.upgrade()?,
        context.owner_node?,
    ))
}

pub(crate) fn context_origin(context: &BrowsingContext) -> Origin {
    context.root_or_child_origin()
}

pub(crate) fn metadata_origin(metadata: &RealmMetadata) -> Origin {
    metadata.origin.borrow().clone()
}

pub(crate) fn require_same_origin_context(ctx: &mut Ctx, context: &BrowsingContext) -> OpResult<()> {
    let caller = ctx.invocation_host_realm();
    if metadata_for_realm(ctx, &caller).is_some_and(|metadata| {
        metadata.origin.borrow().same_origin(&context.root_or_child_origin())
    }) { return Ok(()); }
    Err(crate::error_reporting::dom_exception(ctx, "SecurityError", "Cross-origin Location access is forbidden"))
}

pub(crate) fn focus_window_context(ctx: &mut Ctx, context: &BrowsingContext, blur: bool) -> OpResult<()> {
    // Window.blur() has no focus or reporting side effects. A retired Window
    // likewise has no active navigable on which to run the focusing steps.
    if blur || !context.is_active() {return Ok(());}
    let Some(document)=context.document() else{return Ok(());};
    if !super::focus::allow_focus_with_context(Some(ctx),&document) {return Ok(());}
    // Navigable focusing resolves to its active document's viewport. Reuse the
    // maintained owner chain and focus-event dispatch, including realm entry.
    super::focus::focus_parent_chain(ctx,&document)?;
    document.focus(ctx,None)
}

pub(crate) fn is_active_document(context: &BrowsingContext, document: &DomRealm) -> bool {
    context.is_active()
        && context
            .document
            .borrow()
            .as_ref()
            .is_some_and(|active| core::ptr::eq(active.as_ref(), document))
}

pub(crate) fn context_parent_or_self(context: &Rc<BrowsingContext>) -> Rc<BrowsingContext> {
    context.parent_context().unwrap_or_else(|| context.clone())
}

pub(crate) fn context_frame_count(context: &BrowsingContext) -> usize {
    context.connected_iframe_nodes().len()
}

pub(crate) fn context_realm_handle(context: &BrowsingContext) -> RealmHandle {
    context.realm_handle()
}

pub(crate) fn context_same_origin(left: &BrowsingContext, right: &BrowsingContext) -> bool {
    left.same_origin_with(right)
}

pub(crate) fn context_document(context: &BrowsingContext) -> Option<Rc<DomRealm>> {
    context.document()
}

pub(crate) fn active_child_realm_handle(
    context: &BrowsingContext,
    document: &DomRealm,
) -> OpResult<Option<RealmHandle>> {
    if context.parent.is_none() && context.group.upgrade().is_some_and(|group| {
        context.realm.borrow().global().object_identity() == Some(group.root_key)
    }) {
        return Ok(None);
    }
    if !context.active.get() {
        return Err(OpError::new(
            "InvalidStateError",
            "document belongs to an inactive browsing context",
        ));
    }
    let active_document = context.document.borrow();
    if active_document
        .as_ref()
        .is_some_and(|active| core::ptr::eq(active.as_ref(), document))
    {
        Ok(Some(context.realm_handle()))
    } else {
        Err(OpError::new(
            "InvalidStateError",
            "document is no longer the active document of its browsing context",
        ))
    }
}

pub(crate) fn update_document_origin(context: &BrowsingContext, url: &str) {
    context.set_document_origin_from_url(url);
}

pub(crate) fn bind_context_document(context: &Rc<BrowsingContext>, realm: &Rc<DomRealm>) {
    *realm.browsing_context.borrow_mut() = Rc::downgrade(context);
    *context.document.borrow_mut() = Some(realm.clone());
    if let Some(group) = context.group.upgrade() { group.track_document(realm, context.source_bytes.get()); }
}

pub(crate) fn admit_parser_source(realm: &Rc<DomRealm>, source_bytes: usize) -> OpResult<()> {
    let Some(context) = realm.browsing_context() else { return Ok(()); };
    let Some(group) = context.group.upgrade() else { return Ok(()); };
    let limits = group.limits.get();
    let weak = Rc::downgrade(realm);
    let mut bytes = source_bytes.saturating_add(group.staged_source_bytes.get());
    for record in group.documents.borrow().iter() {
        if !record.document.ptr_eq(&weak) && record.document.strong_count() != 0 { bytes = bytes.saturating_add(record.source_bytes); }
    }
    if source_bytes > limits.max_document_source_bytes || bytes > limits.max_total_source_bytes {
        return Err(OpError::new("QuotaExceededError", "Browsing-context parser source budget exceeded"));
    }
    group.track_document(realm, source_bytes);
    Ok(())
}

pub(crate) fn register_context_service(ctx: &mut Ctx, context: Rc<BrowsingContext>) {
    RealmServices::replace_current(ctx, context_service(context));
}

pub(crate) fn register_context_service_with_metadata(
    ctx: &mut Ctx,
    context: Rc<BrowsingContext>,
    metadata: Rc<RealmMetadata>,
) {
    let registry = context
        .metadata_registry()
        .expect("active browsing context group");
    RealmServices::replace_current(
        ctx,
        BrowsingContextService {
            context: Rc::downgrade(&context),
            metadata,
            registry,
        },
    );
}

pub(crate) fn create_context_proxy(ctx: &mut Ctx, context: &BrowsingContext) -> OpResult<Value> {
    context
        .make_window_proxy(ctx)
        .map_err(|error| OpError::error(error.to_string()))
}

pub(crate) fn current_realm_context(ctx: &mut Ctx) -> Option<Rc<BrowsingContext>> {
    RealmServices::<BrowsingContextService>::current(ctx)
        .and_then(|service| service.context.upgrade())
}

pub(crate) fn current_realm_metadata(ctx: &mut Ctx) -> Option<Rc<RealmMetadata>> {
    RealmServices::<BrowsingContextService>::current(ctx).map(|service| service.metadata.clone())
}

pub(crate) fn metadata_for_realm(ctx: &mut Ctx, realm: &RealmHandle) -> Option<Rc<RealmMetadata>> {
    ctx.op_state().get::<ContextGroupRegistry>()?.groups.values()
        .find_map(|group| group.metadata.for_realm(realm))
}

/// This dedicated Window operation is cross-origin callable. It never broadens
/// the native receiver permission used by document, history or scrolling APIs.
pub(crate) fn message_receiver_context(ctx: &mut Ctx, receiver: &Value) -> Option<Rc<BrowsingContext>> {
    let identity = receiver.object_identity()?;
    ctx.op_state().get::<ContextGroupRegistry>()?.groups.values().find_map(|group| {
        group.contexts.borrow().values().find(|context| {
            context.proxy().and_then(|proxy| proxy.object_identity()) == Some(identity)
                || context.realm.borrow().global().object_identity() == Some(identity)
        }).cloned()
    })
}

pub(crate) fn window_parent_from_metadata(ctx: &mut Ctx, metadata: &RealmMetadata) -> Value {
    metadata
        .parent_proxy
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .or_else(|| {
            metadata
                .self_proxy
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
        })
        .unwrap_or_else(|| ctx.global_this_value())
}

pub(crate) fn window_top_from_metadata(ctx: &mut Ctx, metadata: &RealmMetadata) -> Value {
    metadata
        .top_proxy
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .or_else(|| {
            metadata
                .self_proxy
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
        })
        .unwrap_or_else(|| ctx.global_this_value())
}

pub(crate) fn window_self_from_metadata(ctx: &mut Ctx, metadata: &RealmMetadata) -> Value {
    metadata
        .self_proxy
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
        .unwrap_or_else(|| ctx.global_this_value())
}

pub(crate) fn window_frame_element_from_metadata(
    ctx: &mut Ctx,
    metadata: &RealmMetadata,
) -> OpResult<Value> {
    let Some(parent_metadata) = metadata
        .parent_metadata
        .as_ref()
        .and_then(std::rc::Weak::upgrade)
    else {
        return Ok(Value::Null);
    };
    if !metadata
        .origin
        .borrow()
        .same_origin(&parent_metadata.origin.borrow())
    {
        return Ok(Value::Null);
    }
    let Some(owner_realm) = metadata
        .owner_realm
        .as_ref()
        .and_then(std::rc::Weak::upgrade)
    else {
        return Ok(Value::Null);
    };
    let Some(owner_node) = metadata.owner_node else {
        return Ok(Value::Null);
    };
    let Some(parent_context) = owner_realm.browsing_context() else {
        return Ok(Value::Null);
    };
    let parent_realm = parent_context.realm_handle();
    ctx.with_host_realm(&parent_realm, |ctx| owner_realm.wrap(ctx, owner_node))
        .map_err(host_realm_error)
}

pub(crate) fn invocation_origin(ctx: &mut Ctx) -> Option<Origin> {
    let service = RealmServices::<BrowsingContextService>::current(ctx)?;
    let caller = ctx.invocation_host_realm();
    service
        .registry
        .for_realm(&caller)
        .map(|metadata| metadata.origin.borrow().clone())
}

pub(crate) fn window_parent_value(ctx: &mut Ctx, context: &Rc<BrowsingContext>) -> Value {
    let target = context_parent_or_self(context);
    context_proxy(ctx, &target).unwrap_or(Value::Null)
}

pub(crate) fn window_top_value(ctx: &mut Ctx, context: &Rc<BrowsingContext>) -> Value {
    let target = context_top(context);
    context_proxy(ctx, &target).unwrap_or(Value::Null)
}

pub(crate) fn window_frames_value(ctx: &mut Ctx, context: &Rc<BrowsingContext>) -> Value {
    context_proxy(ctx, context).unwrap_or(Value::Null)
}

pub(crate) fn window_length(context: &BrowsingContext) -> u32 {
    context_frame_count(context).min(u32::MAX as usize) as u32
}

pub(crate) fn window_frame_element(ctx: &mut Ctx, context: &BrowsingContext) -> OpResult<Value> {
    let Some((parent, owner_realm, node)) = context_frame_element(context) else {
        return Ok(Value::Null);
    };
    if !context_same_origin(context, &parent) {
        return Ok(Value::Null);
    }
    let parent_realm = context_realm_handle(&parent);
    ctx.with_host_realm(&parent_realm, |ctx| owner_realm.wrap(ctx, node))
        .map_err(host_realm_error)
}

pub(crate) fn ensure_frame_for_node(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
) -> OpResult<Option<FrameContext>> {
    if !realm.has_browsing_context {
        return Ok(None);
    }
    let connected = {
        let session = realm.session.borrow();
        script_loading::is_connected(session.document(), node)
    };
    if !connected {
        return Ok(None);
    }
    realm.ensure_frame_context(ctx, node).map(Some)
}

pub(crate) fn host_realm_error(_: HostRealmScopeError) -> OpError {
    OpError::type_error("browsing context realm is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid browser test source") {
            Ok(value)=>value,
            Err(exception)=> {
                let message=engine.ctx().coerce_string(&exception).ok().map(|text|text.to_string()).unwrap_or_else(||"unprintable exception".into());
                panic!("browser test script threw {message}; source: {source}");
            }
        }
    }

    #[test]
    fn auxiliary_window_identity_names_close_and_async_javascript() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let mut engine = runtime.engine();
        let parent = crate::install(engine.ctx(), "<head><base href='/assets/'></head><body></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        assert!(matches!(eval(&mut engine, r#"
            globalThis.effects = 0;
            globalThis.popup = open('', 'native-popup');
            popup !== null && popup.opener === window && popup.parent === popup &&
            popup.top === popup && popup.document.baseURI === 'https://parent.example.test/assets/' &&
            open('', 'native-popup') === popup && window.length === 0 && !popup.closed
        "#), Value::Bool(true)));
        assert_eq!(parent.auxiliary_contexts().len(), 1);
        assert!(matches!(eval(&mut engine, r#"
            popup.location.href = 'javascript:opener.effects += 1'; effects === 0
        "#), Value::Bool(true)));
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "effects === 1 && popup.document.URL === 'about:blank'"), Value::Bool(true)));
        assert!(matches!(eval(&mut engine, "popup.opener = null; popup.close(); popup.closed && popup.opener === null"), Value::Bool(true)));
        assert!(parent.auxiliary_contexts().is_empty());
        assert!(matches!(eval(&mut engine, "window.close(); !window.closed && open('', '_blank', 'noopener') === null"), Value::Bool(true)));
        assert_eq!(parent.auxiliary_contexts().len(), 1);
    }

    #[test]
    fn auxiliary_javascript_navigation_checks_actual_target_policy() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let mut engine = runtime.engine();
        let parent = crate::install(engine.ctx(), "<body></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        assert!(matches!(eval(&mut engine, "globalThis.effects = 0; globalThis.popup = open('', 'policy-popup'); true"), Value::Bool(true)));
        let frame = parent.auxiliary_contexts().remove(0);
        let request = frame.request_host_navigation("https://parent.example.test/strict").unwrap();
        frame.install_response_for_request(engine.ctx(), &request,
            "https://parent.example.test/strict", "text/html",
            "<head><meta http-equiv='Content-Security-Policy' content=\"script-src 'none'\"></head><body></body>", 128).unwrap();
        let result = engine.eval_value_in_host_realm(&frame.realm_handle(),
            "globalThis.violations=[]; addEventListener('securitypolicyviolation', e => violations.push([e.violatedDirective,e.blockedURI,e.isTrusted])); true", false).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
        assert!(matches!(eval(&mut engine, "popup.location.href = 'javascript:opener.effects += 1'; true"), Value::Bool(true)));
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "effects === 0"), Value::Bool(true)));
        let result = engine.eval_value_in_host_realm(&frame.realm_handle(),
            "violations.length === 1 && violations[0][0] === 'script-src-elem' && violations[0][1] === 'inline' && violations[0][2]", false).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
    }

    #[test]
    fn auxiliary_javascript_navigation_unsafe_hashes_executes_or_reports() {
        for (policy, allowed) in [
            ("script-src 'unsafe-hashes' 'sha256-IIiAJ8UuliU8o1qAv6CV4P3R8DeTf/v3MrsCwXW171Y='", true),
            ("script-src 'sha256-IIiAJ8UuliU8o1qAv6CV4P3R8DeTf/v3MrsCwXW171Y='", false),
            ("script-src 'unsafe-hashes' 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='", false),
        ] {
            let mut runtime = lumen_runtime::Runtime::new_browser();
            let mut engine = runtime.engine();
            let html = format!("<head><meta http-equiv='Content-Security-Policy' content=\"{policy}\"></head><body></body>");
            let parent = crate::install(engine.ctx(), &html, 128).unwrap();
            parent.set_document_url("https://parent.example.test/page");
            assert!(matches!(eval(&mut engine, r#"
                globalThis.messages=[]; globalThis.violations=[];
                addEventListener('message', e => messages.push([e.data,e.source===popup,e.origin]));
                addEventListener('securitypolicyviolation', e => violations.push([e.violatedDirective,e.blockedURI,e.isTrusted]));
                globalThis.popup=open("javascript:opener.postMessage('pass', '*')");
                messages.length===0
            "#), Value::Bool(true)));
            for _ in 0..3 { assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty()); }
            let expression = if allowed {
                "messages.length===1 && messages[0][0]==='pass' && messages[0][1] && messages[0][2]==='https://parent.example.test' && violations.length===0"
            } else {
                "messages.length===0 && violations.length===1 && violations[0][0]==='script-src-elem' && violations[0][1]==='inline' && violations[0][2]"
            };
            assert!(matches!(eval(&mut engine, expression), Value::Bool(true)), "policy: {policy}");
        }
    }

    #[test]
    fn auxiliary_javascript_string_completion_commits_real_document() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let mut engine = runtime.engine();
        let parent = crate::install(engine.ctx(), "<body></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        assert!(matches!(eval(&mut engine,
            "globalThis.popup=open('', 'replacement'); popup.location.href=\"javascript:'<body><p id=result>actual replacement</p></body>'\"; popup.document.querySelector('#result')===null"), Value::Bool(true)));
        let frame = parent.auxiliary_contexts().remove(0);
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        let request = frame.navigation_request();
        let FrameSource::JavaScriptDocument {source,url} = &request.source else {panic!("actual string completion must request a document")};
        assert_eq!(url, "about:blank");
        assert_eq!(source, "<body><p id=result>actual replacement</p></body>");
        frame.install_response_for_request(engine.ctx(), &request, url, "text/html", source, 128).unwrap();
        assert!(matches!(eval(&mut engine,
            "popup===open('', 'replacement') && popup.document.querySelector('#result').textContent==='actual replacement' && popup.document.URL==='about:blank' && popup.parent===popup && popup.opener===window"), Value::Bool(true)));
    }

    fn set_attribute(realm: &Rc<DomRealm>, node: NodeId, name: &str, value: &str) {
        realm.with_session(|session| {
            session
                .document_mut()
                .set_attribute(node, name, value)
                .expect("valid test attribute");
        });
    }

    fn node_by_id(realm: &Rc<DomRealm>, wanted: &str) -> NodeId {
        let session = realm.session.borrow();
        let document = session.document();
        let root = document.root();
        let mut current = document.first_child(root).expect("root child lookup");
        while let Some(node) = current {
            if document
                .get_attribute_ns(node, None, "id")
                .expect("id lookup")
                .as_deref()
                == Some(wanted)
            {
                return node;
            }
            current = lumen_html::selector::next_descendant(document, root, node)
                .expect("document traversal");
        }
        panic!("no element with id {wanted}");
    }

    fn active_child_handle(document: &DomRealm) -> RealmHandle {
        match document.child_realm_handle() {
            Ok(Some(handle)) => handle,
            Ok(None) => panic!("expected child browsing context"),
            Err(_) => panic!("expected active child document"),
        }
    }

    #[test]
    fn frame_details_adoption_survives_source_retirement_gc_and_keeps_task_order() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(),
            "<iframe id='child' srcdoc='<details id=\"moving\"></details>'></iframe>", 128).unwrap();
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let child = frame.current_document().unwrap();
        let child_handle = active_child_handle(&child);
        eval(&mut engine, "globalThis.frameAdoptionSteps=[];globalThis.normalChildTaskRan=false;globalThis.recordFrameAdoptionStep=step=>frameAdoptionSteps.push(step);");
        let queue_parent_step = |engine: &mut Engine, step: &'static str| {
            super::super::scheduling::queue_task(engine.ctx(), move |ctx| {
                let global = ctx.global_object();
                let record = ctx.member_get(&global, "recordFrameAdoptionStep").map_err(OpError::thrown)?;
                lumen::embed::JsFunction::from_value(record).unwrap().call(ctx, global, &[Value::str(step)])?;
                Ok(())
            }).unwrap();
        };
        queue_parent_step(&mut engine, "before");
        assert!(engine.eval_value_in_host_realm(&child_handle, r#"(() => {
            const d = document.getElementById('moving');
            d.ontoggle = function(e) {
                const parentWindow = this.ownerDocument.defaultView;
                parentWindow.frameAdoptionSteps.push(e instanceof parentWindow.ToggleEvent && e.isTrusted &&
                    e.target === this && e.currentTarget === this &&
                    e.oldState === 'closed' && e.newState === 'open' ? 'toggle' : 'bad-toggle');
            };
            d.open = true;
        })()"#, false).unwrap().is_ok());
        let parent_global = engine.ctx().global_object();
        engine.ctx().with_host_realm(&child_handle, |ctx| {
            super::super::scheduling::queue_task(ctx, move |ctx| {
                ctx.member_set(&parent_global, "normalChildTaskRan", Value::Bool(true)).map_err(OpError::thrown)
            }).unwrap();
        }).unwrap();
        assert!(matches!(eval(&mut engine, "globalThis.adoptedFrameDetailsProbe=document.adoptNode(document.getElementById('child').contentDocument.getElementById('moving'));adoptedFrameDetailsProbe.ownerDocument===document"), Value::Bool(true)));
        let value = eval(&mut engine, "adoptedFrameDetailsProbe");
        let target = engine.ctx().weak_value(&value).unwrap();
        drop(value);
        eval(&mut engine, "adoptedFrameDetailsProbe=null");
        queue_parent_step(&mut engine, "after");
        parent.with_session(|session| session.document_mut().remove(frame.owner_node()).unwrap());
        let retired = parent.take_detached_frame_realms(engine.ctx());
        assert_eq!(retired.len(), 1);
        assert!(frame.current_document().is_none(), "source frame document is retired");
        for realm in retired { dispose_compat_realm(engine.ctx(), &realm); }
        drop(child_handle);
        drop(child);
        drop(frame);
        engine.collect_garbage();
        assert!(target.upgrade().is_some(), "transferred native task retains target through GC");
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "!normalChildTaskRan&&frameAdoptionSteps.join(',')==='before,toggle,after'"), Value::Bool(true)));
        assert!(!super::super::scheduling::task_pending(engine.ctx()));
        engine.collect_garbage();
        assert!(target.upgrade().is_none(), "delivered adopted target is released");
    }

    #[test]
    fn frame_details_initial_and_staged_parser_transitions_use_child_tasks() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(),
            "<iframe id='child' srcdoc='<details open></details>'></iframe>", 128).unwrap();
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let initial = frame.current_document().unwrap();
        let script = "globalThis.frameToggles=[];document.querySelector('details').addEventListener('toggle',e=>frameToggles.push(e instanceof ToggleEvent&&e.isTrusted&&e.oldState==='closed'&&e.newState==='open'&&e.target.ownerDocument===document));";
        let initial_handle = active_child_handle(&initial);
        assert!(engine.eval_value_in_host_realm(&initial_handle, script, false).unwrap().is_ok());
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(engine.eval_value_in_host_realm(&initial_handle,
            "frameToggles.length===1&&frameToggles[0]", false).unwrap().ok().unwrap(), Value::Bool(true)));
        for (mime, source) in [
            ("text/html", "<details open></details>"),
            ("application/xhtml+xml", "<details xmlns='http://www.w3.org/1999/xhtml' open=''/>")
        ] {
            let request = frame.navigation_request();
            let child = frame.install_response_for_request(engine.ctx(), &request,
                "about:srcdoc", mime, source, 128).unwrap();
            let handle = active_child_handle(&child);
            assert!(engine.eval_value_in_host_realm(&handle, script, false).unwrap().is_ok());
            engine.collect_garbage();
            assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
            assert!(matches!(engine.eval_value_in_host_realm(&handle,
                "frameToggles.length===1&&frameToggles[0]", false).unwrap().ok().unwrap(), Value::Bool(true)));
        }
    }

    #[test]
    fn frame_details_failed_parse_and_retirement_preserve_parent_work() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<iframe id='child'></iframe>", 128).unwrap();
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let request = frame.navigation_request();
        assert!(matches!(frame.prepare_response_for_request(engine.ctx(), &request,
            "about:blank", "application/xhtml+xml",
            "<details xmlns='http://www.w3.org/1999/xhtml' open=''><bad></details>", 128),
            Err(FrameInstallError::Parse(_))));
        assert!(!super::super::scheduling::task_pending(engine.ctx()));
        let mut prepared = frame.prepare_response_for_request(engine.ctx(), &request,
            "about:blank", "text/html", "<details open></details>", 128).unwrap();
        assert!(!super::super::scheduling::task_pending(engine.ctx()));
        // A host is allowed to pump while providers/document remain staged.
        // The already admitted real notification must wait for its native lease.
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(!super::super::scheduling::task_pending(engine.ctx()));
        frame.initialize_prepared_document(engine.ctx(), &mut prepared).unwrap();
        assert!(super::super::scheduling::task_pending(engine.ctx()));
        let handle = prepared.realm_handle();
        assert!(engine.eval_value_in_host_realm(&handle,
            "globalThis.stagedDetailsCount=0;document.querySelector('details').addEventListener('toggle',()=>stagedDetailsCount++);",
            false).unwrap().is_ok());
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(engine.eval_value_in_host_realm(&handle,
            "stagedDetailsCount===1", false).unwrap().ok().unwrap(), Value::Bool(true)));
        assert!(engine.eval_value_in_host_realm(&handle,
            "document.querySelector('details').removeAttribute('open');", false).unwrap().is_ok());
        super::super::scheduling::queue_task(engine.ctx(), |ctx| {
            let global = ctx.global_object();
            ctx.member_set(&global, "parentDetailsWork", Value::Bool(true)).map_err(OpError::thrown)
        }).unwrap();
        for realm in prepared.discard(engine.ctx()) { dispose_compat_realm(engine.ctx(), &realm); }
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "parentDetailsWork===true"), Value::Bool(true)));
        assert!(!super::super::scheduling::task_pending(engine.ctx()));
    }

    #[test]
    fn specification_window_discard_retires_unqualified_task_owner_only_for_staging_realm() {
        let mut engine=Engine::new();
        // Establish the creator's tuple origin before the iframe is born. A
        // pending same-origin network navigation leaves initial about:blank
        // active without a src mutation that would mature an about navigation.
        let parent=crate::install(engine.ctx(),"<body></body>",128).unwrap();
        parent.set_document_url("https://pending.test/parent");
        assert!(matches!(eval(&mut engine,"const pendingFrame=document.createElement('iframe');pendingFrame.src='https://pending.test/unloaded';document.body.append(pendingFrame);true"),Value::Bool(true)));
        let frame=parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let request=frame.navigation_request();
        let prepared=frame.prepare_response_for_request(engine.ctx(),&request,"https://pending.test/unloaded","text/html","<details></details>",128).unwrap();
        assert!(prepared.reuses_window());
        let staging=prepared.staging_realm.as_ref().unwrap().clone();
        let sender=engine.ctx().with_host_realm(&staging,super::super::scheduling::task_sender).unwrap().unwrap();
        let called=Rc::new(Cell::new(false));
        let retired_called=called.clone();
        assert!(sender.queue(move |_|{retired_called.set(true);Ok(())}).is_ok());
        super::super::scheduling::queue_task(engine.ctx(),|ctx|{
            let global=ctx.global_object();
            ctx.member_set(&global,"survivingParentTask",Value::Bool(true)).map_err(OpError::thrown)
        }).unwrap();
        let retired=prepared.discard(engine.ctx());
        assert!(!sender.is_live());
        assert!(super::super::scheduling::run_tasks(&mut engine,16).is_empty());
        assert!(!called.get());
        assert!(matches!(eval(&mut engine,"survivingParentTask===true"),Value::Bool(true)));
        assert!(frame.current_document().is_some());
        for realm in retired { engine.ctx().dispose_host_realm(&realm).expect("dispose discarded staging realm"); }
    }

    #[test]
    fn origins_use_tuple_identity_and_distinct_opaque_tokens() {
        assert_eq!(
            Origin::from_url("https://example.test/a"),
            Origin::from_url("https://example.test/b")
        );
        assert_ne!(
            Origin::from_url("https://example.test/a"),
            Origin::from_url("http://example.test/a")
        );
        let first = Origin::opaque();
        let second = Origin::opaque();
        assert_ne!(first, second);
        assert_eq!(first.serialize(), "null");
        assert_eq!(second.serialize(), "null");
    }

    #[test]
    fn local_navigation_captures_policy_and_pageshow_without_inheriting_tuple_origin() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let parent = crate::install(engine.ctx(), "<iframe></iframe>", 128).unwrap();
        parent.set_document_url("https://creator.test/page");
        parent.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "img-src 'self'".into())]).unwrap();
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        assert!(!frame.current_document().unwrap().module_fetch_policy_snapshot().unwrap().check("https://creator.test/image", "about:blank", lumen_common::csp::Destination::Image).unwrap().blocked);
        set_attribute(&parent, frame.owner_node(), "src", "data:text/html,child");
        let request = frame.navigation_request();
        // A change after ingress cannot silently change the request snapshot.
        parent.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "img-src 'none'".into())]).unwrap();
        let child = frame.install_response_for_request(engine.ctx(), &request,
            "data:text/html,child", "text/html", "<body></body>", 128).unwrap();
        assert_eq!(frame.origin().serialize(), "null");
        assert!(!frame.origin().same_origin(&Origin::from_url("https://creator.test/")));
        let policies = child.module_fetch_policy_snapshot().unwrap();
        assert!(!policies.check("https://creator.test/image", "data:text/html,child", lumen_common::csp::Destination::Image).unwrap().blocked);
        assert!(policies.check("https://foreign.test/image", "data:text/html,child", lumen_common::csp::Destination::Image).unwrap().blocked);
        let handle = frame.realm_handle();
        let result = engine.eval_value_in_host_realm(&handle, "globalThis.shows=[];addEventListener('pageshow', e=>shows.push(e.isTrusted && !e.persisted));true", false).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
        engine.ctx().with_host_realm(&handle, |ctx| child.dispatch_window_user_agent(ctx, "load", false, false)).unwrap().unwrap();
        let result = engine.eval_value_in_host_realm(&handle, "shows.length===1 && shows[0]", false).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
    }

    #[test]
    fn srcdoc_inherits_creator_origin_and_effective_base() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<head><base href='https://assets.example.test/app/'></head><body><iframe srcdoc='child'></iframe></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let expected_base = parent.base_url();
        let parent_origin = Origin::from_url("https://parent.example.test/page");

        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize connected iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let request = frame.navigation_request();
        assert!(matches!(request.source, FrameSource::SrcDoc(_)));
        assert_eq!(frame.origin(), parent_origin);
        let initial = frame.current_document().expect("initial empty document");
        assert_eq!(initial.document_url().as_deref(), Some("about:blank"));
        assert_eq!(initial.base_url(), expected_base);
        let child = frame.install_response_for_request(engine.ctx(), &request,
            "about:srcdoc", "text/html", "child", 128).unwrap();
        assert!(!Rc::ptr_eq(&initial, &child));
        assert_eq!(child.document_url().as_deref(), Some("about:srcdoc"));
        assert_eq!(child.base_url(), expected_base);

        set_attribute(&parent, frame.owner_node(), "sandbox", "");
        let sandboxed_request = frame.navigation_request();
        assert_eq!(
            sandboxed_request.unsupported,
            Some(FrameUnsupportedReason::SandboxPolicy)
        );
        assert_eq!(frame.origin(), parent_origin);
    }

    #[test]
    fn explicit_about_blank_response_inherits_origin_and_base_but_keeps_its_url() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<head><base href='https://assets.example.test/app/'></head><body><iframe id='child'></iframe></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let expected_base = parent.base_url();
        let parent_origin = Origin::from_url("https://parent.example.test/page");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize connected iframe")
            .into_iter()
            .next()
            .expect("one iframe");

        set_attribute(&parent, frame.owner_node(), "src", "about:blank#child");
        let request = frame.navigation_request();
        assert!(matches!(
            &request.source,
            FrameSource::Url(url) if is_about_blank_url(url)
        ));
        let mut prepared = frame
            .prepare_response_for_request(
                engine.ctx(),
                &request,
                "about:blank#child",
                "text/html",
                "",
                128,
            )
            .expect("prepare explicit about:blank response");
        assert_eq!(prepared.origin(), parent_origin);
        frame
            .initialize_prepared_document(engine.ctx(), &mut prepared)
            .expect("initialize blank document");
        let committed = frame
            .commit_prepared_response(engine.ctx(), &mut prepared)
            .expect("commit current blank response");
        assert_eq!(
            committed.document.document_url().as_deref(),
            Some("about:blank#child")
        );
        assert_eq!(committed.document.base_url(), expected_base);
        assert_eq!(frame.origin(), parent_origin);
    }

    #[test]
    fn initial_frame_realm_installs_runtime_providers_before_dom() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let parent = crate::install(
            engine.ctx(),
            "<body><iframe id='child'></iframe></body>",
            128,
        )
        .unwrap();
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let handle = frame.realm_handle();
        let completion = match engine.ctx().with_host_realm(&handle, |ctx| {
                let global = ctx.global_object();
                ctx.eval_in_realm(
                    &global,
                    "globalThis === window && self === window && typeof URL === 'function' && typeof TextDecoder === 'function' && typeof FormData === 'function' && document instanceof Document",
                )
            })
        {
            Ok(completion) => completion,
            Err(_) => panic!("enter initialized iframe realm"),
        };
        let value = match completion {
            Ok(value) => value,
            Err(_) => panic!("child bootstrap did not throw"),
        };
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn captured_blank_window_proxy_survives_cross_origin_navigation_and_message_relay() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let mut engine = runtime.engine();
        let parent = crate::install(engine.ctx(), "<body><iframe id=child></iframe></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        assert!(matches!(eval(&mut engine, r#"
            window.savedChild = document.getElementById('child').contentWindow;
            window.received = [];
            addEventListener('message', event => {
                received.push([event.source === savedChild, event.origin, event.data]);
                if (event.data === 'ready') event.source.postMessage('reply', '*');
            });
            true
        "#), Value::Bool(true)));
        set_attribute(&parent, frame.owner_node(), "src", "https://guest.example.test/child");
        let request = frame.navigation_request();
        let child = frame.install_response_for_request(engine.ctx(), &request,
            "https://guest.example.test/child", "text/html", "<body>guest</body>", 128).unwrap();
        assert!(matches!(eval(&mut engine,
            "savedChild === document.getElementById('child').contentWindow"), Value::Bool(true)));
        let handle = active_child_handle(&child);
        let result = engine.eval_value_in_host_realm(&handle, r#"
            addEventListener('message', event => {
                if (event.source === parent && event.origin === 'https://parent.example.test')
                    parent.postMessage(event.data, '*');
            });
            parent.postMessage('ready', '*');
            true
        "#, false).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))));
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(matches!(eval(&mut engine, "received.length === 1 && received[0][0] && received[0][2] === 'ready'"), Value::Bool(true)));
        // The queue deliberately snapshots one turn; each posted message is a
        // further task, rather than recursively draining tasks created by it.
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        assert!(super::super::scheduling::run_tasks(&mut engine, 16).is_empty());
        let received = eval(&mut engine, "JSON.stringify(received)");
        let received = engine.ctx().coerce_string(&received).ok().expect("relay JSON is a string");
        assert!(matches!(eval(&mut engine, r#"
            received.length === 2 && received[0][0] && received[1][0] &&
            received[0][1] === 'https://guest.example.test' &&
            received[1][1] === 'https://guest.example.test' &&
            received[0][2] === 'ready' && received[1][2] === 'reply'
        "#), Value::Bool(true)), "actual Window relay: {received}");
    }

    #[test]
    fn navigation_retargets_stable_proxy_and_detach_preserves_retained_proxy() {
        let mut engine = Engine::new();
        let parent =
            crate::install(engine.ctx(), "<body><iframe id=child></iframe></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let initial_handle = frame.realm_handle();
        let proxy = frame.window_proxy().expect("initial WindowProxy");
        let initial_document = frame
            .current_document()
            .expect("initial child document is active");
        assert!(matches!(parent.child_realm_handle(), Ok(None)));
        assert_eq!(
            active_child_handle(&initial_document)
                .global()
                .object_identity(),
            initial_handle.global().object_identity()
        );
        assert_eq!(
            frame.origin(),
            Origin::from_url("https://parent.example.test/page")
        );

        assert!(matches!(
            eval(
                &mut engine,
                "window.savedChild = document.getElementById('child').contentWindow; window.savedChildDocument = document.getElementById('child').contentDocument; savedChild.marker = 17; true",
            ),
            Value::Bool(true)
        ));
        set_attribute(
            &parent,
            frame.owner_node(),
            "src",
            "https://parent.example.test/child",
        );
        let request = frame.navigation_request();
        let child = frame
            .install_response_for_request(
                engine.ctx(),
                &request,
                "https://parent.example.test/child",
                "text/html",
                "<body>new child</body>",
                128,
            )
            .expect("install same-origin response");
        assert!(initial_document.child_realm_handle().is_err());
        assert_eq!(
            active_child_handle(&child).global().object_identity(),
            frame.realm_handle().global().object_identity()
        );
        let current_proxy = frame.window_proxy().expect("retargeted WindowProxy");
        assert!(engine.ctx().values_strict_equal(&proxy, &current_proxy));
        assert_ne!(
            initial_handle.global().object_identity(),
            frame.realm_handle().global().object_identity()
        );
        assert_eq!(
            child.document_url().as_deref(),
            Some("https://parent.example.test/child")
        );
        assert!(matches!(
            eval(
                &mut engine,
                "savedChild === document.getElementById('child').contentWindow && savedChild.marker === undefined",
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(
                &mut engine,
                "document.location === window.location && savedChildDocument.location === null && document.getElementById('child').contentDocument.location !== null",
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(&mut engine, "savedChild.marker = 41; true"),
            Value::Bool(true)
        ));
        let active_request = frame.navigation_request();
        assert!(frame.request_is_current(&active_request));

        parent.with_session(|session| {
            session
                .document_mut()
                .remove(frame.owner_node())
                .expect("detach iframe");
        });
        assert_eq!(parent.retire_detached_frame_contexts(engine.ctx()), 1);
        assert!(frame.current_document().is_none());
        assert!(!frame.request_is_current(&active_request));
        let group = frame.inner.group.upgrade().expect("test group handle");
        assert_eq!(
            group.contexts.borrow().len(),
            1,
            "only the top context remains active"
        );
        drop(child);
        drop(initial_handle);
        drop(frame);
        assert!(matches!(
            eval(
                &mut engine,
                "savedChild.marker === 41 && savedChild.parent === window && savedChild.top === window",
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn prepared_responses_stay_unpublished_until_current_request_commits() {
        let mut engine = Engine::new();
        let parent =
            crate::install(engine.ctx(), "<body><iframe id=child></iframe></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let active_realm = frame.realm_handle();
        let active_document = frame.current_document().expect("initial document");
        let proxy = frame.window_proxy().expect("stable WindowProxy");

        set_attribute(
            &parent,
            frame.owner_node(),
            "src",
            "https://parent.example.test/child-a",
        );
        let request_a = frame.navigation_request();
        let mut stale = frame
            .prepare_response_for_request(
                engine.ctx(),
                &request_a,
                "https://parent.example.test/child-a",
                "text/html",
                "<body>stale</body>",
                128,
            )
            .expect("prepare response without publishing it");
        let staged_realm = stale.realm_handle();
        assert_ne!(
            active_realm.global().object_identity(),
            staged_realm.global().object_identity()
        );
        assert!(stale.document_realm().is_none());
        assert!(Rc::ptr_eq(
            &active_document,
            &frame.current_document().expect("active document unchanged")
        ));
        assert!(engine
            .ctx()
            .values_strict_equal(&proxy, &frame.window_proxy().expect("same proxy")));
        assert!(matches!(
            frame.commit_prepared_response(engine.ctx(), &mut stale),
            Err(FrameInstallError::NotInitialized)
        ));
        assert!(Rc::ptr_eq(
            &active_document,
            &frame
                .current_document()
                .expect("uninitialized response is unpublished")
        ));

        frame
            .initialize_prepared_document(engine.ctx(), &mut stale)
            .expect("initialize after the host-provider phase");
        assert!(stale.document_realm().is_some());
        assert!(Rc::ptr_eq(
            &active_document,
            &frame
                .current_document()
                .expect("initial document still active")
        ));

        set_attribute(
            &parent,
            frame.owner_node(),
            "src",
            "https://parent.example.test/child-b",
        );
        assert!(matches!(
            frame.commit_prepared_response(engine.ctx(), &mut stale),
            Err(FrameInstallError::StaleRequest)
        ));
        let discarded = stale.discard(engine.ctx());
        assert!(discarded.iter().any(|realm| {
            realm.global().object_identity() == staged_realm.global().object_identity()
        }));
        for realm in discarded {
            let _ = engine.ctx().dispose_host_realm(&realm);
        }
        assert!(Rc::ptr_eq(
            &active_document,
            &frame
                .current_document()
                .expect("stale response did not replace document")
        ));
        assert_eq!(
            active_realm.global().object_identity(),
            frame.realm_handle().global().object_identity()
        );

        let current = frame.navigation_request();
        let mut prepared = frame
            .prepare_response_for_request(
                engine.ctx(),
                &current,
                "https://parent.example.test/child-b",
                "text/html",
                "<body>current</body>",
                128,
            )
            .expect("prepare current response");
        frame
            .initialize_prepared_document(engine.ctx(), &mut prepared)
            .expect("initialize current document");
        let committed = frame
            .commit_prepared_response(engine.ctx(), &mut prepared)
            .expect("publish current response");
        assert!(Rc::ptr_eq(
            &committed.document,
            &frame.current_document().expect("new document is active")
        ));
        assert!(committed.retired_realms.iter().any(|realm| {
            realm.global().object_identity() == active_realm.global().object_identity()
        }));
        assert!(engine.ctx().values_strict_equal(
            &proxy,
            &frame.window_proxy().expect("proxy identity retained")
        ));
        assert_eq!(
            frame.realm_handle().global().object_identity(),
            prepared.realm_handle().global().object_identity()
        );
        drop(prepared);
        for realm in committed.retired_realms {
            let _ = engine.ctx().dispose_host_realm(&realm);
        }
    }

    #[test]
    fn discarded_prepared_realms_do_not_accumulate_weak_metadata_entries() {
        let mut engine = Engine::new();
        let parent =
            crate::install(engine.ctx(), "<body><iframe id=child></iframe></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let group = frame.inner.group.upgrade().expect("live context group");
        let live_metadata_count = group.metadata.realms.borrow().len();

        for _ in 0..24 {
            let request = frame.navigation_request();
            let prepared = frame
                .prepare_response_for_request(
                    engine.ctx(),
                    &request,
                    "about:blank",
                    "text/html",
                    "<body>discarded</body>",
                    128,
                )
                .expect("prepare transient child realm");
            for realm in prepared.discard(engine.ctx()) {
                super::scheduling::cancel_tasks_for_realm(engine.ctx(), &realm);
                let _ = engine.ctx().dispose_host_realm(&realm);
            }
            assert!(
                group.metadata.realms.borrow().len() <= live_metadata_count + 1,
                "only the latest discarded metadata entry may remain until the next registration"
            );
        }
    }

    #[test]
    fn frame_admission_collects_unreachable_retired_realms_under_budget_pressure() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<body></body>", 512).unwrap();
        parent.set_document_url("https://example.test/");
        parent.set_frame_resource_limits(FrameResourceLimits { max_realms: 4, ..FrameResourceLimits::default() }).unwrap();
        for index in 0..32 {
            assert!(matches!(eval(&mut engine, "globalThis.frame=document.createElement('iframe');document.body.append(frame);true"), Value::Bool(true)));
            let contexts = parent.frame_contexts(engine.ctx()).unwrap_or_else(|error| panic!("frame {index}: {error:?}"));
            assert_eq!(contexts.len(), 1);
            drop(contexts);
            assert!(matches!(eval(&mut engine, "frame.remove();frame=null;true"), Value::Bool(true)));
            assert_eq!(parent.retire_detached_frame_contexts(engine.ctx()), 1);
        }
        assert!(parent.frame_contexts(engine.ctx()).unwrap().is_empty());
        assert_eq!(parent.browsing_context().unwrap().group.upgrade().unwrap().limits.get().max_realms, 4);
    }

    #[test]
    fn frame_admission_still_charges_author_retained_detached_documents() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<body></body>", 512).unwrap();
        parent.set_document_url("https://example.test/");
        parent.set_frame_resource_limits(FrameResourceLimits { max_realms: 4, ..FrameResourceLimits::default() }).unwrap();
        eval(&mut engine, "globalThis.saved=[];true");
        for _ in 0..3 {
            eval(&mut engine, "globalThis.frame=document.createElement('iframe');document.body.append(frame);saved.push(frame.contentDocument);true");
            drop(parent.frame_contexts(engine.ctx()).unwrap());
            eval(&mut engine, "frame.remove();frame=null;true");
            assert_eq!(parent.retire_detached_frame_contexts(engine.ctx()), 1);
        }
        eval(&mut engine, "globalThis.frame=document.createElement('iframe');document.body.append(frame);true");
        let error = parent.frame_contexts(engine.ctx()).err().expect("retained documents still exhaust the configured budget");
        assert_eq!(error.class(), "QuotaExceededError");
        assert!(matches!(eval(&mut engine, "saved.length===3 && saved[0].body!==null"), Value::Bool(true)));
    }

    #[test]
    fn host_cleanup_before_reinserted_frame_materialization_preserves_the_live_context() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<body><main id=main><iframe id=child></iframe></main></body>",
            128,
        )
        .unwrap();
        let old = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let identity = old.weak_identity();
        let owner = old.owner_node();
        let main = node_by_id(&parent, "main");
        parent.with_session(|session| {
            session.document_mut().remove(owner).unwrap();
            session.document_mut().append(main, owner).unwrap();
        });
        let retired = parent.take_detached_frame_realms(engine.ctx());
        assert_eq!(retired.len(), 1);
        let replacement = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        assert!(!identity.matches(&replacement));
        assert!(replacement.current_document().is_some());
        assert!(replacement.request_is_current(&replacement.navigation_request()));
        assert!(old.current_document().is_none());
        assert!(parent.take_detached_frame_realms(engine.ctx()).is_empty());
        for realm in retired {
            dispose_compat_realm(engine.ctx(), &realm);
        }
    }

    #[test]
    fn synchronous_frame_detach_and_reinsert_retires_one_navigable() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<body><main id=main><iframe id=child></iframe></main></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let old_frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let old_realm = old_frame.realm_handle();
        let old_proxy = old_frame.window_proxy().expect("old WindowProxy");
        let old_identity = old_frame.weak_identity();
        assert!(old_identity.matches(&old_frame));
        let owner = old_frame.owner_node();
        let pre_detach_request = old_frame.navigation_request();
        assert!(matches!(
            eval(
                &mut engine,
                "window.retainedFrame = document.getElementById('child').contentWindow; true",
            ),
            Value::Bool(true)
        ));

        let main = node_by_id(&parent, "main");
        parent.with_session(|session| {
            session.document_mut().remove(owner).expect("detach iframe");
            session
                .document_mut()
                .append(main, owner)
                .expect("reinsert iframe in the same script turn");
        });
        assert!(!old_frame.request_is_current(&pre_detach_request));

        let new_frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize the new iframe navigable")
            .into_iter()
            .next()
            .expect("one reinserted iframe");
        assert!(old_frame.current_document().is_none());
        assert!(!old_identity.matches(&new_frame));
        assert_ne!(
            old_realm.global().object_identity(),
            new_frame.realm_handle().global().object_identity()
        );
        let new_proxy = new_frame.window_proxy().expect("new WindowProxy");
        assert!(!engine.ctx().values_strict_equal(&old_proxy, &new_proxy));

        let retired = parent.take_detached_frame_realms(engine.ctx());
        assert_eq!(retired.len(), 1);
        assert_eq!(
            retired[0].global().object_identity(),
            old_realm.global().object_identity()
        );
        assert!(parent.take_detached_frame_realms(engine.ctx()).is_empty());
        assert!(matches!(
            eval(&mut engine, "retainedFrame.parent === window"),
            Value::Bool(true)
        ));
        for realm in retired {
            super::scheduling::cancel_tasks_for_realm(engine.ctx(), &realm);
            let _ = engine.ctx().dispose_host_realm(&realm);
        }
        drop(old_frame);
        drop(new_frame);
        assert!(matches!(
            eval(&mut engine, "retainedFrame.parent === window"),
            Value::Bool(true)
        ));
    }

    #[test]
    fn navigation_input_epochs_reject_a_to_b_to_a_and_ignore_namespaced_attributes() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<head><base id=active href='/one/'><base id=inactive href='/unused/'></head><body><iframe id=child src='page.html'></iframe></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.example.test/index.html");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let owner = frame.owner_node();
        let initial = frame.navigation_request();
        let initial_source = initial.source.clone();

        set_attribute(&parent, owner, "src", "other.html");
        assert!(!frame.request_is_current(&initial));
        set_attribute(&parent, owner, "src", "page.html");
        let returned_to_initial_source = frame.navigation_request();
        assert_eq!(returned_to_initial_source.source, initial_source);
        assert_ne!(returned_to_initial_source.generation, initial.generation);
        assert!(!frame.request_is_current(&initial));

        let before_namespaced = frame.navigation_request();
        parent.with_session(|session| {
            session
                .document_mut()
                .set_attribute_ns(owner, Some("urn:custom"), "src", "custom.html")
                .expect("set namespaced lookalike");
        });
        assert_eq!(frame.navigation_generation(), before_namespaced.generation);
        assert!(frame.request_is_current(&before_namespaced));

        set_attribute(&parent, owner, "sandbox", "");
        assert!(!frame.request_is_current(&before_namespaced));
        let sandbox_request = frame.navigation_request();
        set_attribute(&parent, owner, "srcdoc", "");
        assert!(!frame.request_is_current(&sandbox_request));
        assert_eq!(
            frame.navigation_request().source,
            FrameSource::SrcDoc(String::new())
        );
    }

    #[test]
    fn host_navigation_uses_document_base_and_preserves_iframe_source_attributes() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<head><base href='/forms/'></head><body><iframe id=child></iframe></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://forms.example.test/dir/index.html");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let owner = frame.owner_node();
        let before_generation = frame.navigation_generation();
        let before_attributes = parent.with_session(|session| {
            let document = session.document();
            (
                document
                    .get_attribute_ns(owner, None, "src")
                    .expect("read src"),
                document
                    .get_attribute_ns(owner, None, "srcdoc")
                    .expect("read srcdoc"),
            )
        });

        let first = frame
            .request_host_navigation("submit?result=ok")
            .expect("resolve relative host navigation");
        assert_eq!(
            first.source,
            FrameSource::Url("https://forms.example.test/forms/submit?result=ok".into())
        );
        assert_ne!(first.generation, before_generation);
        assert!(frame.request_is_current(&first));
        let after_attributes = parent.with_session(|session| {
            let document = session.document();
            (
                document
                    .get_attribute_ns(owner, None, "src")
                    .expect("read src"),
                document
                    .get_attribute_ns(owner, None, "srcdoc")
                    .expect("read srcdoc"),
            )
        });
        assert_eq!(after_attributes, before_attributes);

        set_attribute(&parent, owner, "src", "fallback.html");
        assert!(!frame.request_is_current(&first));
        assert_eq!(
            frame.navigation_request().source,
            FrameSource::Url("https://forms.example.test/forms/fallback.html".into())
        );

        let second = frame
            .request_host_navigation("second")
            .expect("request a later host navigation");
        assert!(!frame.request_is_current(&first));
        assert!(frame.request_is_current(&second));
        frame.inner.active.set(false);
        assert!(frame.request_host_navigation("inactive").is_err());
    }

    #[test]
    fn specification_window_attribute_source_snapshots_and_detach_epochs() {
        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<head><base id=active href='/one/'><base id=inactive href='/ignored/'></head><body><main id=main><iframe id=child src='page.html'></iframe></main></body>",
            128,
        )
        .unwrap();
        parent.set_document_url("https://parent.example.test/index.html");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        let original = frame.navigation_request();

        set_attribute(&parent, node_by_id(&parent, "active"), "href", "/two/");
        assert!(frame.request_is_current(&original));
        assert_eq!(frame.navigation_request(), original);
        set_attribute(&parent, frame.owner_node(), "src", "page.html?next");
        let second_base = frame.navigation_request();
        assert_eq!(second_base.source, FrameSource::Url("https://parent.example.test/two/page.html?next".into()));
        assert!(!frame.request_is_current(&original));
        set_attribute(&parent, node_by_id(&parent, "active"), "href", "/one/");
        assert!(frame.request_is_current(&second_base));
        let back_to_original = frame.navigation_request();
        assert_eq!(back_to_original.source, second_base.source);
        assert_ne!(back_to_original.generation, original.generation);

        parent.set_document_url("https://parent.example.test/new-location/document.html");
        assert!(frame.request_is_current(&back_to_original));
        let after_document_url_change = frame.navigation_request();
        set_attribute(&parent, node_by_id(&parent, "inactive"), "href", "/three/");
        assert!(frame.request_is_current(&after_document_url_change));

        let owner = frame.owner_node();
        let before_detach = frame.navigation_request();
        parent.with_session(|session| {
            session.document_mut().remove(owner).expect("detach iframe");
        });
        assert!(!frame.request_is_current(&before_detach));
        let detached_request = frame.navigation_request();
        let container = node_by_id(&parent, "main");
        parent.with_session(|session| {
            session
                .document_mut()
                .append(container, owner)
                .expect("reinsert iframe");
        });
        assert!(!frame.request_is_current(&detached_request));
        assert_eq!(frame.navigation_request().source, detached_request.source);
    }

    #[test]
    fn specification_window_text_document_navigation_preserves_mime_and_literal_source() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<iframe></iframe>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/index.html");
        let frame = parent.frame_contexts(engine.ctx()).unwrap().remove(0);
        let source = "<script>globalThis.executed=true</script>\r\n{\"value\":42}";
        for mime in ["application/json", "application/problem+json", "text/javascript1.2", "text/vtt"] {
            let request = frame.request_host_navigation("https://parent.example.test/resource").unwrap();
            let document = frame.install_response_for_request(engine.ctx(), &request,
                "https://parent.example.test/resource", mime, source, 128).unwrap();
            assert_eq!(document.content_type,mime);
            let result = engine.eval_value_in_host_realm(&frame.realm_handle(),
                r#"document.compatMode==='CSS1Compat' && document.querySelector('script')===null && typeof executed==='undefined' && document.querySelector('pre').textContent==="<script>globalThis.executed=true</script>\n{\"value\":42}""#,false)
                .unwrap().ok().expect("inspect actual text document");
            assert!(matches!(result,Value::Bool(true)),"{mime}");
        }
    }

    #[test]
    fn cross_origin_proxy_denies_document_access_with_security_error() {
        let mut engine = Engine::new();
        let parent =
            crate::install(engine.ctx(), "<body><iframe id=child></iframe></body>", 128).unwrap();
        parent.set_document_url("https://parent.example.test/page");
        let frame = parent
            .frame_contexts(engine.ctx())
            .expect("materialize iframe")
            .into_iter()
            .next()
            .expect("one iframe");
        set_attribute(
            &parent,
            frame.owner_node(),
            "src",
            "https://other.example.test/child",
        );
        let request = frame.navigation_request();
        frame
            .install_response_for_request(
                engine.ctx(),
                &request,
                "https://other.example.test/child",
                "text/html",
                "<body>cross-origin</body>",
                128,
            )
            .expect("install cross-origin response");
        assert_ne!(
            frame.origin(),
            Origin::from_url("https://parent.example.test/page")
        );
        assert!(matches!(
            eval(
                &mut engine,
                "try { document.getElementById('child').contentWindow.document; false } catch (error) { error.name === 'SecurityError' }",
            ),
            Value::Bool(true)
        ));
    }
}
