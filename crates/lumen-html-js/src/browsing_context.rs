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

#[derive(Debug)]
pub enum FrameInstallError {
    StaleRequest,
    Unsupported(FrameUnsupportedReason),
    UnsupportedContentType(String),
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
        self.realms
            .borrow_mut()
            .insert(key, Rc::downgrade(metadata));
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
    active: Cell<bool>,
    navigation_generation: Cell<u64>,
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

impl FrameContext {
    pub fn owner_node(&self) -> NodeId {
        self.inner
            .owner_node
            .expect("frame browsing contexts have an owner iframe")
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

    /// Check a captured fetch request before installing its response. A later
    /// source/base mutation invalidates an older request without retargeting the
    /// WindowProxy until a current response is ready.
    pub fn request_is_current(&self, request: &FrameNavigationRequest) -> bool {
        self.inner.active.get() && request == &self.navigation_request()
    }

    pub fn navigation_generation(&self) -> u64 {
        self.inner.navigation_generation.get()
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
        if !self.inner.active.get() || !self.request_is_current(request) {
            return Err(FrameInstallError::StaleRequest);
        }
        if let Some(reason) = &request.unsupported {
            return Err(FrameInstallError::Unsupported(reason.clone()));
        }

        let (mime, document, is_html) = parse_frame_document(source, content_type, max_nodes)?;
        let old_realm = self.inner.realm.borrow().clone();
        let old_document = self.inner.document.borrow().clone();
        let old_metadata = self.inner.metadata.borrow().clone();
        let group = self
            .inner
            .group
            .upgrade()
            .ok_or(FrameInstallError::HostRealm)?;
        let new_realm = ctx.create_host_realm();
        let proxy = self.window_proxy().ok_or(FrameInstallError::HostRealm)?;
        let inherits_creator =
            matches!(request.source, FrameSource::Blank | FrameSource::SrcDoc(_));
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
        *self.inner.metadata.borrow_mut() = new_metadata.clone();
        *self.inner.proxy_metadata.borrow_mut() = new_metadata.clone();
        if ctx.retarget_window_proxy(&proxy, &new_realm).is_err() {
            *self.inner.metadata.borrow_mut() = old_metadata.clone();
            *self.inner.proxy_metadata.borrow_mut() = old_metadata;
            let _ = ctx.dispose_host_realm(&new_realm);
            return Err(FrameInstallError::HostRealm);
        }
        group.unregister_active(&old_realm);
        *self.inner.realm.borrow_mut() = new_realm.clone();
        group.register(&self.inner);

        let result = ctx.with_host_realm(&new_realm, |ctx| {
            register_context_service(ctx, self.inner.clone());
            super::install_document(
                ctx,
                document,
                &mime,
                is_html,
                true,
                Some(self.inner.clone()),
                Some(final_url.to_owned()),
                inherited_base,
            )
        });
        let realm = match result {
            Ok(Ok(realm)) => realm,
            _ => {
                let _ = ctx.retarget_window_proxy(&proxy, &old_realm);
                group.unregister_active(&new_realm);
                *self.inner.realm.borrow_mut() = old_realm.clone();
                *self.inner.metadata.borrow_mut() = old_metadata.clone();
                *self.inner.proxy_metadata.borrow_mut() = old_metadata;
                group.register(&self.inner);
                let _ = ctx.dispose_host_realm(&new_realm);
                return Err(FrameInstallError::HostRealm);
            }
        };

        self.inner.inherit_about_origin.set(inherits_creator);
        let old_document = self
            .inner
            .document
            .replace(Some(realm.clone()))
            .or(old_document);
        self.inner
            .navigation_generation
            .set(self.inner.navigation_generation.get().wrapping_add(1));
        if let Some(old_document) = old_document {
            let mut retired = Vec::new();
            old_document.retire_all_frame_contexts(ctx, &mut retired);
            for retired_realm in retired {
                let _ = ctx.dispose_host_realm(&retired_realm);
            }
        }
        let _ = ctx.dispose_host_realm(&old_realm);
        Ok(realm)
    }
}

fn parse_frame_document(
    source: &str,
    content_type: &str,
    max_nodes: usize,
) -> Result<(String, lumen_html::Document, bool), FrameInstallError> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches(|ch: char| ch.is_ascii_whitespace())
        .to_ascii_lowercase();
    match essence.as_str() {
        "text/html" => {
            let document = html::parse_with_declarative_shadow_roots(source, max_nodes, true)
                .map_err(|error| FrameInstallError::Parse(InstallError::Parse(error)))?;
            Ok((essence, document, true))
        }
        "application/xhtml+xml" => {
            let document = lumen_html::xml::parse(source, max_nodes)
                .map_err(|error| FrameInstallError::Parse(InstallError::XmlParse(error)))?;
            Ok((essence, document, false))
        }
        "application/xml" | "text/xml" | "image/svg+xml" => {
            let document = lumen_html::xml::parse(source, max_nodes)
                .map_err(|error| FrameInstallError::Parse(InstallError::XmlParse(error)))?;
            Ok((essence, document, false))
        }
        _ => Err(FrameInstallError::UnsupportedContentType(essence)),
    }
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
    let base_url = owner_realm.base_url();
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
    let source = if let Some(srcdoc) = srcdoc {
        FrameSource::SrcDoc(srcdoc)
    } else if let Some(src) = src {
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
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
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
                    }) if name == "iframe"
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
        self.retire_detached_frame_contexts(ctx);
        let connected = {
            let session = self.session.borrow();
            script_loading::is_connected(session.document(), node)
        };
        if !self.has_browsing_context || !connected {
            return Err(OpError::type_error("iframe has no active browsing context"));
        }
        if let Some(context) = self
            .frame_contexts
            .borrow()
            .get(&node)
            .and_then(std::rc::Weak::upgrade)
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
        let document = html::parse_with_declarative_shadow_roots(&source, 65_536, true)
            .map_err(|error| OpError::error(format!("{error:?}")))?;
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
            active: Cell::new(true),
            navigation_generation: Cell::new(0),
        });
        let proxy = match child.make_window_proxy(ctx) {
            Ok(proxy) => proxy,
            Err(_) => {
                let _ = ctx.dispose_host_realm(&child_realm);
                return Err(OpError::error("failed to create iframe WindowProxy"));
            }
        };
        *metadata.self_proxy.borrow_mut() = ctx.weak_value(&proxy);
        group.register(&child);
        let frame = FrameContext {
            inner: child.clone(),
        };
        self.frame_contexts
            .borrow_mut()
            .insert(node, Rc::downgrade(&child));

        let inherited_base = self.base_url();
        let install = ctx.with_host_realm(&child_realm, |ctx| {
            register_context_service(ctx, child.clone());
            super::install_document(
                ctx,
                document,
                "text/html",
                true,
                true,
                Some(child.clone()),
                Some(url.to_owned()),
                Some(inherited_base),
            )
        });
        match install {
            Ok(Ok(realm)) => {
                *child.document.borrow_mut() = Some(realm);
                let _ = proxy;
                Ok(frame)
            }
            _ => {
                self.frame_contexts.borrow_mut().remove(&node);
                group.unregister_active(&child_realm);
                let _ = ctx.dispose_host_realm(&child_realm);
                Err(OpError::error("failed to install iframe initial document"))
            }
        }
    }

    /// Materialize connected iframe children in tree order for an embedder's
    /// navigation and load-event pump. A contentWindow getter is not required
    /// for host lifecycle processing.
    pub fn frame_contexts(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<Vec<FrameContext>> {
        self.retire_detached_frame_contexts(ctx);
        let nodes = self.connected_iframe_nodes();
        nodes
            .into_iter()
            .map(|node| self.ensure_frame_context(ctx, node))
            .collect()
    }

    /// Release host ownership of iframe navigables whose owner element has been detached.
    /// Called from the host's frame/load pump so document mutations need no engine callback.
    pub fn retire_detached_frame_contexts(&self, ctx: &mut Ctx) -> usize {
        let connected = self.connected_iframe_nodes();
        let retired: Vec<Rc<BrowsingContext>> = {
            let mut cache = self.frame_contexts.borrow_mut();
            let mut retired = Vec::new();
            cache.retain(|node, weak| {
                let Some(context) = weak.upgrade() else {
                    return false;
                };
                if connected.contains(node) && context.active.get() {
                    return true;
                }
                retired.push(context);
                false
            });
            retired
        };
        let count = retired.len();
        let mut realm_handles = Vec::new();
        for context in retired {
            context.retire(ctx, &mut realm_handles);
        }
        for realm in realm_handles {
            let _ = ctx.dispose_host_realm(&realm);
        }
        count
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
        assert_eq!(
            frame.origin(),
            Origin::from_url("https://parent.example.test/page")
        );

        assert!(matches!(
            eval(
                &mut engine,
                "window.savedChild = document.getElementById('child').contentWindow; savedChild.marker = 17; true",
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
