//! Native browsing-context and iframe WindowProxy ownership.
//!
//! A URL is only one input to an origin. In particular, initial `about:blank` and
//! `about:srcdoc` documents inherit their creator's origin while keeping their own
//! document URL and inherited base URL.

use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::{
    HostRealmScopeError, RealmHandle, WeakValue, WindowProxyDisposition, WindowProxyOperation,
    WindowProxyPolicy, WindowProxyResult,
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
    /// A present `src` attribute resolved against the effective document base.
    Url(String),
    /// Neither source attribute is present; the initial document is `about:blank`.
    Blank,
    /// A present `src` could not be parsed by the shared URL implementation.
    InvalidUrl(String),
}

/// A host-observable snapshot for starting or validating one iframe navigation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameNavigationRequest {
    pub owner: NodeId,
    pub source: FrameSource,
    pub base_url: String,
    pub generation: u64,
    pub unsupported: Option<FrameUnsupportedReason>,
}

/// An iframe response whose DOM and host realm are ready but whose stable
/// WindowProxy has not yet been retargeted. The embedder installs its realm
/// providers before committing this token.
pub struct PreparedFrameResponse {
    context: Rc<BrowsingContext>,
    request: FrameNavigationRequest,
    realm: RealmHandle,
    parsed_document: Option<lumen_html::Document>,
    details_controller: Rc<super::dialog_popover::DetailsController>,
    document: Option<Rc<DomRealm>>,
    mime: String,
    is_html: bool,
    final_url: String,
    inherited_base: Option<String>,
    metadata: Rc<RealmMetadata>,
    inherits_creator: bool,
    committed: bool,
}

impl PreparedFrameResponse {
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
            document.retire_all_frame_contexts(ctx, &mut retired);
        }
        retired.push(self.realm);
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
    /// Strong ownership here is intentional: only active navigables belong to the host group.
    /// Detached/replaced contexts are removed and their realm handles are then released.
    contexts: RefCell<HashMap<usize, Rc<BrowsingContext>>>,
    metadata: Rc<RealmMetadataRegistry>,
    root_key: usize,
}

impl ContextGroup {
    fn new(root_key: usize) -> Rc<Self> {
        Rc::new(Self {
            contexts: RefCell::new(HashMap::new()),
            metadata: Rc::new(RealmMetadataRegistry::default()),
            root_key,
        })
    }

    fn register(&self, context: &Rc<BrowsingContext>) {
        let key = context.realm.borrow().global().object_identity();
        if let Some(key) = key {
            let metadata = context.metadata.borrow().clone();
            metadata.key.set(key);
            self.metadata.register(&metadata);
            self.contexts.borrow_mut().insert(key, context.clone());
        }
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
    group: std::rc::Weak<ContextGroup>,
    metadata: RefCell<Rc<RealmMetadata>>,
    proxy_metadata: Rc<RefCell<Rc<RealmMetadata>>>,
    realm: RefCell<RealmHandle>,
    window_proxy: RefCell<Option<WeakValue>>,
    document: RefCell<Option<Rc<DomRealm>>>,
    parent: Option<std::rc::Weak<BrowsingContext>>,
    owner_realm: Option<std::rc::Weak<DomRealm>>,
    owner_node: Option<NodeId>,
    inherit_about_origin: Cell<bool>,
    /// A Location API navigation takes precedence over reflected `src`/`srcdoc`
    /// until one of those author-controlled iframe inputs changes.
    location_navigation: RefCell<Option<String>>,
    active: Cell<bool>,
    navigation_generation: Cell<u64>,
    initial_blank_load_dispatched: Cell<bool>,
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
}

impl FrameContext {
    pub fn weak_identity(&self) -> WeakFrameIdentity {
        WeakFrameIdentity {
            inner: Rc::downgrade(&self.inner),
        }
    }

    pub fn owner_node(&self) -> NodeId {
        self.inner
            .owner_node
            .expect("frame browsing contexts have an owner iframe")
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
        owner.flush_layout()?;
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

    pub fn current_document(&self) -> Option<Rc<DomRealm>> {
        self.inner
            .active
            .get()
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
        let entry_base_url = self
            .inner
            .document()
            .map(|document| document.base_url())
            .unwrap_or_else(|| self.inner.current_document_url());
        self.inner
            .request_location_navigation_from(input, &entry_base_url)?;
        Ok(self.navigation_request())
    }

    /// Check a captured fetch request before installing its response. A later
    /// source/base mutation invalidates an older request without retargeting the
    /// WindowProxy until a current response is ready.
    pub fn request_is_current(&self, request: &FrameNavigationRequest) -> bool {
        self.inner.active.get() && request == &self.navigation_request()
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
        if !matches!(self.navigation_request().source, FrameSource::Blank) {
            return false;
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
        if !self.inner.active.get() || !self.request_is_current(request) {
            return Err(FrameInstallError::StaleRequest);
        }
        if let Some(reason) = &request.unsupported {
            return Err(FrameInstallError::Unsupported(reason.clone()));
        }

        let _group = self
            .inner
            .group
            .upgrade()
            .ok_or(FrameInstallError::HostRealm)?;
        let _proxy = self.window_proxy().ok_or(FrameInstallError::HostRealm)?;
        let new_realm = ctx.create_host_realm();
        let source = source.into();
        let parsed = ctx.with_host_realm(&new_realm, |ctx| {
            let controller = super::dialog_popover::DetailsController::prepare(ctx)
                .map_err(|_| FrameInstallError::HostRealm)?;
            let document = parse_frame_document(source, content_type, final_url, max_nodes, &controller)?;
            Ok::<_, FrameInstallError>((document, controller))
        });
        let ((mime, document, is_html), details_controller) = match parsed {
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
        let explicit_about_blank = matches!(
            &request.source,
            FrameSource::Url(source_url)
                if is_about_blank_url(source_url) && is_about_blank_url(final_url)
        );
        let inherits_creator =
            matches!(request.source, FrameSource::Blank | FrameSource::SrcDoc(_))
                || explicit_about_blank;
        let origin = if inherits_creator {
            self.inner
                .parent_context()
                .map(|parent| parent.root_or_child_origin())
                .unwrap_or_else(Origin::opaque)
        } else {
            Origin::from_url(final_url)
        };
        let new_metadata = self.inner.new_realm_metadata(ctx, &new_realm, origin);
        let inherited_base = inherits_creator.then(|| {
            self.inner
                .parent_context()
                .and_then(|parent| parent.document())
                .map(|document| document.base_url())
                .unwrap_or_else(|| "about:blank".to_owned())
        });
        Ok(PreparedFrameResponse {
            context: self.inner.clone(),
            request: request.clone(),
            realm: new_realm,
            parsed_document: Some(document),
            details_controller,
            document: None,
            mime,
            is_html,
            final_url: final_url.to_owned(),
            inherited_base,
            metadata: new_metadata,
            inherits_creator,
            committed: false,
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
        if !self.inner.active.get() || !self.request_is_current(&prepared.request) {
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
            )
        });
        let document = match result {
            Ok(Ok(document)) => document,
            _ => return Err(FrameInstallError::HostRealm),
        };
        prepared.document = Some(document.clone());
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

        let group = self
            .inner
            .group
            .upgrade()
            .ok_or(FrameInstallError::HostRealm)?;
        let proxy = self.window_proxy().ok_or(FrameInstallError::HostRealm)?;
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
        *self.inner.realm.borrow_mut() = prepared.realm.clone();
        *self.inner.metadata.borrow_mut() = prepared.metadata.clone();
        *self.inner.proxy_metadata.borrow_mut() = prepared.metadata.clone();
        group.register(&self.inner);
        self.inner
            .inherit_about_origin
            .set(prepared.inherits_creator);
        let old_document = self.inner.document.replace(Some(document.clone()));
        self.inner
            .navigation_generation
            .set(self.inner.navigation_generation.get().wrapping_add(1));

        let mut retired_realms = Vec::new();
        if let Some(old_document) = old_document {
            old_document.retire_all_frame_contexts(ctx, &mut retired_realms);
        }
        retired_realms.push(old_realm);
        prepared.committed = true;
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
) -> Result<(String, lumen_html::Document, bool), FrameInstallError> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .to_ascii_lowercase();
    match essence.as_str() {
        "text/html" => {
            let document = html::parse_with_options_initialized(&source, max_nodes,
                html::ParseOptions { allow_declarative_shadow_roots: true, ..Default::default() },
                |document| controller.attach(document))
                .map_err(|error| FrameInstallError::Parse(InstallError::Parse(error)))?;
            Ok((essence, document, true))
        }
        "application/xhtml+xml" => {
            let document = lumen_html::xml::parse_initialized(&source, max_nodes, |document| controller.attach(document))
                .map_err(|error| FrameInstallError::Parse(InstallError::XmlParse(error)))?;
            Ok((essence, document, false))
        }
        "application/xml" | "text/xml" | "image/svg+xml" => {
            let document = lumen_html::xml::parse_initialized(&source, max_nodes, |document| controller.attach(document))
                .map_err(|error| FrameInstallError::Parse(InstallError::XmlParse(error)))?;
            Ok((essence, document, false))
        }
        "text/plain" | "text/css" => {
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
            Ok((essence, document, true))
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
            Ok((essence, document, true))
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
        };
    };
    let location_navigation = context.location_navigation.borrow().clone();
    let base_url = location_navigation
        .as_ref()
        .map(|_| context.current_document_url())
        .unwrap_or_else(|| owner_realm.base_url());
    let (srcdoc, src, sandbox) = {
        let session = owner_realm.session.borrow();
        let document = session.document();
        let srcdoc = document
            .get_attribute_ns(owner, None, "srcdoc")
            .ok()
            .flatten();
        let src = document.get_attribute_ns(owner, None, "src").ok().flatten();
        let sandbox = document
            .get_attribute_ns(owner, None, "sandbox")
            .ok()
            .flatten();
        (srcdoc, src, sandbox)
    };
    let source = if let Some(url) = location_navigation {
        FrameSource::Url(url)
    } else if let Some(srcdoc) = srcdoc {
        FrameSource::SrcDoc(srcdoc)
    } else if let Some(src) = src.filter(|src| !src.is_empty()) {
        match lumen_common::url::parse(&src, Some(&base_url)) {
            Ok(url) => FrameSource::Url(url.href()),
            Err(_) => FrameSource::InvalidUrl(src),
        }
    } else {
        FrameSource::Blank
    };
    let sandboxed = sandbox.is_some();
    let sandbox_allows_same_origin = sandbox.as_deref().is_some_and(|value| {
        super::html_space_tokens(value).any(|token| token.eq_ignore_ascii_case("allow-same-origin"))
    });
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
    }
}

impl BrowsingContext {
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
            group: Rc::downgrade(&group),
            metadata: RefCell::new(metadata.clone()),
            proxy_metadata: proxy_metadata.clone(),
            realm: RefCell::new(realm),
            window_proxy: RefCell::new(None),
            document: RefCell::new(None),
            parent: None,
            owner_realm: None,
            owner_node: None,
            inherit_about_origin: Cell::new(false),
            location_navigation: RefCell::new(None),
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
            initial_blank_load_dispatched: Cell::new(false),
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

    fn root_or_child_origin(&self) -> Origin {
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

    pub(crate) fn invalidate_owner_navigation_request(&self) {
        self.location_navigation.borrow_mut().take();
        self.invalidate_navigation_request();
    }

    pub(crate) fn current_document_url(&self) -> String {
        self.document()
            .and_then(|document| document.document_url())
            .unwrap_or_else(|| "about:blank".to_owned())
    }

    pub(crate) fn request_location_navigation(&self, input: &str) -> OpResult<()> {
        self.request_location_navigation_from(input, &self.current_document_url())
    }

    pub(crate) fn request_location_navigation_from(
        &self,
        input: &str,
        entry_base_url: &str,
    ) -> OpResult<()> {
        if !self.active.get() {
            return Err(OpError::new(
                "InvalidStateError",
                "browsing context is no longer active",
            ));
        }
        if self.parent.is_none() {
            return Err(OpError::new(
                "NotSupportedError",
                "top-level navigation is not supported by this host",
            ));
        }
        let url = lumen_common::url::parse(input, Some(entry_base_url))
            .map_err(|_| OpError::new("SyntaxError", "invalid Location URL"))?;
        *self.location_navigation.borrow_mut() = Some(url.href());
        self.navigation_generation
            .set(self.navigation_generation.get().wrapping_add(1));
        Ok(())
    }

    fn realm_handle(&self) -> RealmHandle {
        self.realm.borrow().clone()
    }

    fn document(&self) -> Option<Rc<DomRealm>> {
        self.document.borrow().clone()
    }

    fn proxy(&self) -> Option<Value> {
        self.window_proxy
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
    }

    fn parent_context(&self) -> Option<Rc<BrowsingContext>> {
        self.parent.as_ref()?.upgrade()
    }

    fn frame_element(&self) -> Option<(Rc<DomRealm>, NodeId)> {
        Some((self.owner_realm.as_ref()?.upgrade()?, self.owner_node?))
    }

    fn top_context(self: &Rc<Self>) -> Rc<Self> {
        let mut current = self.clone();
        while let Some(parent) = current.parent_context() {
            current = parent;
        }
        current
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
        self.navigation_generation
            .set(self.navigation_generation.get().wrapping_add(1));
        if let Some(document) = self.document.borrow_mut().take() {
            document.retire_all_frame_contexts(ctx, retired);
        }
        let realm = self.realm_handle();
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
        let parent_proxy = self.parent_context().and_then(|parent| parent.proxy());
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
        _operation: &WindowProxyOperation,
    ) -> WindowProxyDisposition {
        let (Some(caller), Some(target)) =
            (self.metadata_for(caller), self.target_metadata(target))
        else {
            return WindowProxyDisposition::Denied(Self::denied(ctx));
        };
        if caller.origin.borrow().same_origin(&target.origin.borrow()) {
            WindowProxyDisposition::ForwardSameOrigin
        } else {
            WindowProxyDisposition::Denied(Self::denied(ctx))
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
        caller: &RealmHandle,
        target: &RealmHandle,
        index: u32,
    ) -> Option<Value> {
        let group = self.group.upgrade()?;
        let caller_context = self.metadata_for(caller)?;
        let target_context = group.for_realm(target)?;
        if !caller_context
            .origin
            .borrow()
            .same_origin(&target_context.root_or_child_origin())
        {
            return None;
        }
        let owner_realm = target_context.document()?;
        let node = target_context
            .connected_iframe_nodes()
            .get(index as usize)
            .copied()?;
        let target_handle = target_context.realm_handle();
        ctx.with_host_realm(&target_handle, |ctx| {
            owner_realm
                .ensure_frame_context(ctx, node)
                .ok()
                .and_then(|frame| frame.window_proxy())
        })
        .ok()
        .flatten()
    }
}

impl DomRealm {
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
                        .is_some_and(|(_, local_name)| local_name == "iframe")
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

    pub(crate) fn ensure_frame_context(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
    ) -> OpResult<FrameContext> {
        let connected = {
            let session = self.session.borrow();
            script_loading::is_connected(session.document(), node)
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
        let (sandboxed, sandbox_allows_same_origin, srcdoc) = {
            let session = self.session.borrow();
            let document = session.document();
            let sandbox = document
                .get_attribute_ns(node, None, "sandbox")
                .map_err(super::dom_error)?;
            let srcdoc = document
                .get_attribute_ns(node, None, "srcdoc")
                .map_err(super::dom_error)?;
            let allow_same_origin = sandbox.as_deref().is_some_and(|value| {
                super::html_space_tokens(value)
                    .any(|token| token.eq_ignore_ascii_case("allow-same-origin"))
            });
            (sandbox.is_some(), allow_same_origin, srcdoc)
        };
        let (source, url) = match srcdoc {
            Some(source) => (source, "about:srcdoc"),
            None => (String::new(), "about:blank"),
        };
        let origin = if sandboxed && !sandbox_allows_same_origin {
            Origin::opaque()
        } else {
            parent.root_or_child_origin()
        };
        let group = parent
            .group
            .upgrade()
            .ok_or_else(|| OpError::type_error("browsing context group has retired"))?;
        let child_realm = ctx.create_host_realm();
        let parsed = ctx.with_host_realm(&child_realm, |ctx| {
            let controller = super::dialog_popover::DetailsController::prepare(ctx)?;
            let document = html::parse_with_options_initialized(&source, 65_536,
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
            parent_metadata: Some(Rc::downgrade(&parent.metadata.borrow())),
            owner_realm: Some(Rc::downgrade(self)),
            owner_node: Some(node),
        });
        let proxy_metadata = Rc::new(RefCell::new(metadata.clone()));
        let child = Rc::new(BrowsingContext {
            group: Rc::downgrade(&group),
            metadata: RefCell::new(metadata.clone()),
            proxy_metadata: proxy_metadata.clone(),
            realm: RefCell::new(child_realm.clone()),
            window_proxy: RefCell::new(None),
            document: RefCell::new(None),
            parent: Some(Rc::downgrade(&parent)),
            owner_realm: Some(Rc::downgrade(self)),
            owner_node: Some(node),
            inherit_about_origin: Cell::new(true),
            location_navigation: RefCell::new(None),
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
            initial_blank_load_dispatched: Cell::new(false),
        });
        let proxy = match child.make_window_proxy(ctx) {
            Ok(proxy) => proxy,
            Err(_) => {
                dispose_compat_realm(ctx, &child_realm);
                return Err(OpError::error("failed to create iframe WindowProxy"));
            }
        };
        *metadata.self_proxy.borrow_mut() = ctx.weak_value(&proxy);
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
            )
            .map_err(|error| {
                OpError::error(format!(
                    "failed to install iframe initial document: {error:?}"
                ))
            })
        });
        match install {
            Ok(Ok(realm)) => {
                *child.document.borrow_mut() = Some(realm);
                self.frame_contexts
                    .borrow_mut()
                    .insert(node, Rc::downgrade(&child));
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
        self.collect_detached_frame_realms(ctx).1
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

    fn retire_all_frame_contexts(&self, ctx: &mut Ctx, retired: &mut Vec<RealmHandle>) {
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

pub(crate) fn context_service(context: Rc<BrowsingContext>) -> BrowsingContextService {
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

pub(crate) fn context_proxy_value(context: &BrowsingContext) -> Option<Value> {
    context.proxy()
}

pub(crate) fn context_is_top(context: &BrowsingContext) -> bool {
    context.parent.is_none()
}

pub(crate) fn context_parent_option(context: &BrowsingContext) -> Option<Rc<BrowsingContext>> {
    context.parent_context()
}

pub(crate) fn context_top(context: &Rc<BrowsingContext>) -> Rc<BrowsingContext> {
    context.top_context()
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

pub(crate) fn is_active_document(context: &BrowsingContext, document: &DomRealm) -> bool {
    context.active.get()
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
    if context.parent.is_none() {
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

pub(crate) fn current_context(ctx: &mut Ctx) -> Option<Rc<BrowsingContext>> {
    RealmServices::<BrowsingContextService>::current(ctx)
        .and_then(|service| service.context.upgrade())
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

pub(crate) fn invocation_context(ctx: &mut Ctx) -> Option<Rc<BrowsingContext>> {
    let service = RealmServices::<BrowsingContextService>::current(ctx)?;
    let caller = ctx.invocation_host_realm();
    service
        .context
        .upgrade()?
        .group
        .upgrade()?
        .for_realm(&caller)
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
        engine
            .eval_value(source)
            .expect("valid browser test source")
            .ok()
            .expect("browser test script did not throw")
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
        let child = frame.current_document().expect("initial srcdoc document");
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
    fn effective_base_and_detach_reinsert_mutations_advance_frame_request_epoch() {
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
        assert!(!frame.request_is_current(&original));
        let second_base = frame.navigation_request();
        set_attribute(&parent, node_by_id(&parent, "active"), "href", "/one/");
        assert!(!frame.request_is_current(&second_base));
        let back_to_original = frame.navigation_request();
        assert_eq!(back_to_original.source, original.source);
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
