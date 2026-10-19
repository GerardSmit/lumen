//! Source-owned HTML object requests. I/O carries plain captured data; native
//! state owns generation-qualified tasks, representation and document leases.
use super::*;
use lumen_html::object::Representation;
use lumen_html::observe::{ObservedKind, ObservedMutation};
use std::sync::Arc;

const OBJECT_REQUEST_LIMIT: usize = 4096;
pub const OBJECT_BODY_LIMIT: usize = 8 * 1024 * 1024;

pub struct ObjectRequest {
    pub url: String,
    pub destination: lumen_common::csp::Destination,
    pub document_url: String,
    pub origin: String,
    pub referrer: lumen_common::referrer::Referrer,
    pub policies: Arc<lumen_common::csp::PolicySet>,
    pub parser_inserted: bool,
    pub declared_type: String,
    pub local_resource: Option<ObjectLocalResource>,
    pub metadata_reservation: Arc<lumen_common::limits::ByteLease>,
}

/// Captured on the main thread using the shared origin/partition-qualified Blob
/// store. These immutable bytes contain no JS/native document owners.
#[derive(Clone)]
pub struct ObjectLocalResource {
    pub bytes: lumen_common::bytes::Bytes,
    pub mime: String,
    pub creator_origin: Option<browsing_context::Origin>,
    _source_reservation: Arc<lumen_common::limits::ByteLease>,
}

pub struct ObjectResponse {
    pub response: lumen_common::http_body::SyncHttpResponse,
    pub request_referrer: Option<String>,
    pub creator_origin: Option<browsing_context::Origin>,
    pub reservation: lumen_common::limits::ByteLease,
    pub violations: Vec<lumen_common::csp::Violation>,
}

pub struct ObjectFailure {
    pub message: String,
    pub violations: Vec<lumen_common::csp::Violation>,
}

pub trait ObjectResourceLoader {
    fn start(&self, request: ObjectRequest) -> Result<u64, String>;
    fn poll(&self, ticket: u64) -> Option<Result<ObjectResponse, ObjectFailure>>;
    fn cancel(&self, ticket: u64);
}

pub struct ObjectEnvironment {
    factory: Rc<dyn Fn(&mut Ctx) -> Rc<dyn ObjectResourceLoader>>,
    pub documents: Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>,
    images: Rc<RefCell<Vec<std::sync::Weak<lumen_html::paint::ImageData>>>>,
    metadata: Arc<lumen_common::limits::ByteBudget>,
}
impl ObjectEnvironment {
    pub fn new(
        factory: impl Fn(&mut Ctx) -> Rc<dyn ObjectResourceLoader> + 'static,
        documents: Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>,
    ) -> Self {
        Self {
            factory: Rc::new(factory),
            documents,
            images: Rc::new(RefCell::new(Vec::new())),
            metadata: lumen_common::limits::ByteBudget::new(32 * 1024 * 1024),
        }
    }
}

#[derive(Clone)]
struct Selection {
    kind: lumen_html::object::Kind,
    data: Option<Arc<str>>,
    declared_type: Arc<str>,
    base: Arc<str>,
    _metadata: Arc<lumen_common::limits::ByteLease>,
    eligible: bool,
}

struct LoadLease {
    owner: std::rc::Weak<DomRealm>,
}
impl Drop for LoadLease {
    fn drop(&mut self) {
        if let Some(owner) = self.owner.upgrade() {
            owner
                .object_resources
                .load_count
                .set(owner.object_resources.load_count.get() - 1);
        }
    }
}

struct Pending {
    ticket: u64,
    generation: u64,
    _target: ResourceRequestRoot,
    _load: LoadLease,
}

/// The controller consumes the actual response, without a second fetch. This
/// lease remains until its real container load task or cancellation finishes.
pub struct ObjectDocumentResponse {
    pub node: NodeId,
    pub generation: u64,
    pub response: ObjectResponse,
    pub mime: String,
    pending: Pending,
}

#[derive(Default)]
struct Entry {
    parser_created: bool,
    intrinsic_epoch: Option<(u128,(lumen_html::session::TransitionInputEpoch,u64,u64))>,
    embed_previous_connected: bool,
    embed_previous_rendered: bool,
    parser_open: bool,
    dirty: bool,
    task_queued: bool,
    refetch: bool,
    generation: u64,
    selection: Option<Selection>,
    representation: Representation,
    pending: Option<Pending>,
    document_response: Option<ObjectDocumentResponse>,
}

#[derive(Default)]
pub(crate) struct ObjectResources {
    entries: RefCell<HashMap<NodeId, Entry>>,
    provider: RefCell<Option<Rc<dyn ObjectResourceLoader>>>,
    load_count: Cell<usize>,
    metadata: RefCell<Option<Arc<lumen_common::limits::ByteBudget>>>,
    pub(crate) processing_failed: Cell<bool>,
    styles_dirty: Cell<bool>,
    has_embeds: Cell<bool>,
    has_document_representation: Cell<bool>,
    style_epoch: Cell<Option<lumen_html::session::TransitionInputEpoch>>,
    deferred_complete: Cell<bool>,
    deferred_load: Cell<bool>,
    images: RefCell<Rc<RefCell<Vec<std::sync::Weak<lumen_html::paint::ImageData>>>>>,
}
impl ObjectResources {
    pub(crate) fn embed_was_connected(&self,node:NodeId)->bool {
        self.entries.borrow().get(&node).is_some_and(|entry|entry.embed_previous_connected)
    }
}

impl ObjectResources {
    fn metadata_budget(&self) -> Arc<lumen_common::limits::ByteBudget> {
        self.metadata
            .borrow_mut()
            .get_or_insert_with(|| lumen_common::limits::ByteBudget::new(32 * 1024 * 1024))
            .clone()
    }
    fn admit(&self, node: NodeId) -> Result<(), lumen_html::Error> {
        let mut entries = self.entries.borrow_mut();
        if !entries.contains_key(&node) {
            if entries.len() >= OBJECT_REQUEST_LIMIT {
                return Err(lumen_html::Error::LimitExceeded);
            }
            entries
                .try_reserve(1)
                .map_err(|_| lumen_html::Error::LimitExceeded)?;
            entries.insert(node, Entry::default());
        }
        Ok(())
    }

    pub(crate) fn record_birth(&self, document: &lumen_html::Document, node: NodeId, parser: bool) {
        if !lumen_html::object::is_embedded(document, node) {
            return;
        }
        if self.admit(node).is_err() {
            self.processing_failed.set(true);
            return;
        }
        let mut entries = self.entries.borrow_mut();
        let entry = entries.get_mut(&node).expect("admitted object");
        self.has_embeds.set(self.has_embeds.get() || lumen_html::object::kind(document,node)==Some(lumen_html::object::Kind::Embed));
        entry.parser_created |= parser;
        entry.parser_open |= parser && lumen_html::object::is_object(document, node);
        entry.dirty = true;
    }

    pub(crate) fn parser_completed(&self, document: &lumen_html::Document, node: NodeId) {
        if !lumen_html::object::is_embedded(document, node) {
            return;
        }
        if self.admit(node).is_err() {
            self.processing_failed.set(true);
            return;
        }
        let mut entries = self.entries.borrow_mut();
        let entry = entries.get_mut(&node).expect("admitted object");
        entry.parser_open = false;
        entry.dirty = true;
    }

    pub(crate) fn representation(&self, node: NodeId) -> Representation {
        self.entries
            .borrow()
            .get(&node)
            .map_or(Representation::Fallback, |entry| entry.representation)
    }

    fn dirty_subtree(
        &self,
        document: &lumen_html::Document,
        root: NodeId,
    ) -> Result<(), lumen_html::Error> {
        let mut node = root;
        loop {
            if lumen_html::object::is_embedded(document, node) {
                self.has_embeds.set(self.has_embeds.get() || lumen_html::object::kind(document,node)==Some(lumen_html::object::Kind::Embed));
                self.admit(node)?;
                let mut entries = self.entries.borrow_mut();
                let entry = entries.get_mut(&node).expect("admitted object");
                entry.dirty = true;
                entry.refetch |= lumen_html::object::is_object(document, node);
            }
            if let Some(child) = document.first_child(node)? {
                node = child;
                continue;
            }
            loop {
                if node == root {
                    return Ok(());
                }
                if let Some(next) = document.next_sibling(node)? {
                    node = next;
                    break;
                }
                node = document
                    .parent(node)?
                    .ok_or(lumen_html::Error::InvalidNode)?;
            }
        }
    }

    pub(crate) fn observe(
        &self,
        document: &lumen_html::Document,
        mutation: &ObservedMutation,
    ) -> Result<(), lumen_html::Error> {
        match &mutation.kind {
            ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if namespace_uri.is_none() => {
                if lumen_html::object::is_embedded(document, mutation.target) {
                    self.admit(mutation.target)?;
                    let classid = document
                        .get_attribute_ns_ref(mutation.target, None, "classid")?
                        .is_some();
                    let data = document
                        .get_attribute_ns_ref(mutation.target, None, "data")?
                        .is_some();
                    let embed = lumen_html::object::kind(document, mutation.target) == Some(lumen_html::object::Kind::Embed);
                    let relevant = if embed { matches!(name.as_str(), "src" | "type" | "style" | "class" | "hidden") } else { match name.as_str() {
                        "classid" | "style" | "class" | "hidden" => true,
                        "data" => !classid,
                        "type" => !classid && !data,
                        _ => false,
                    }};
                    if relevant {
                        let mut entries = self.entries.borrow_mut();
                        let entry = entries.get_mut(&mutation.target).expect("admitted object");
                        entry.dirty = true;
                        entry.refetch |= matches!(name.as_str(), "classid" | "data" | "src" | "type");
                    }
                }
                // Batch stylesheet eligibility changes over the bounded sparse
                // object inventory at the next native resource checkpoint.
                if matches!(name.as_str(), "style" | "class" | "hidden") {
                    self.styles_dirty.set(true);
                }
            }
            ObservedKind::ChildList { .. } | ObservedKind::ChildListMany { .. } => {
                for root in mutation
                    .kind
                    .added_nodes()
                    .chain(mutation.kind.removed_nodes())
                {
                    if document.kind(root).is_ok() {
                        self.dirty_subtree(document, root)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn retire(&self) {
        if let Some(provider) = self.provider.borrow().as_ref() {
            for entry in self.entries.borrow_mut().values_mut() {
                if let Some(pending) = entry.pending.take() {
                    provider.cancel(pending.ticket);
                }
                entry.document_response = None;
                entry.task_queued = false;
                entry.dirty = false;
            }
        }
    }
}

pub(crate) fn register_document(ctx: &mut Ctx, realm: &Rc<DomRealm>) {
    let weak = Rc::downgrade(realm);
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_parser_element_completion_sink(Some(Rc::new(move |document, node| {
            if let Some(realm) = weak.upgrade() {
                realm.object_resources.parser_completed(document, node);
            }
        })));
    {
        let session = realm.session.borrow();
        if realm
            .object_resources
            .dirty_subtree(session.document(), session.document().root())
            .is_err()
        {
            realm.object_resources.processing_failed.set(true);
        }
    }
    let Some((factory, documents, images, metadata)) = ctx
        .op_state()
        .get::<ObjectEnvironment>()
        .map(|environment| {
            (
                environment.factory.clone(),
                environment.documents.clone(),
                environment.images.clone(),
                environment.metadata.clone(),
            )
        })
    else {
        return;
    };
    if realm.object_resources.provider.borrow().is_none() {
        *realm.object_resources.provider.borrow_mut() = Some(factory(ctx));
    }
    *realm.object_resources.images.borrow_mut() = images;
    *realm.object_resources.metadata.borrow_mut() = Some(metadata);
    let mut documents = documents.borrow_mut();
    documents.retain(|owner| owner.strong_count() != 0);
    if !documents.iter().any(|owner| {
        owner
            .upgrade()
            .is_some_and(|owner| Rc::ptr_eq(&owner, realm))
    }) {
        documents.push(Rc::downgrade(realm));
    }
}

pub(crate) fn content_document(
    ctx: &mut Ctx,
    owner: &Rc<DomRealm>,
    node: NodeId,
    svg_only: bool,
) -> OpResult<Value> {
    let Some(frame) = owner.represented_object_context(node) else {
        return Ok(Value::Null);
    };
    let Some(origin) = browsing_context::invocation_origin(ctx) else {
        return Ok(Value::Null);
    };
    if !origin.same_origin(&frame.origin()) {
        return Ok(Value::Null);
    }
    let Some(document) = frame.current_document() else {
        return Ok(Value::Null);
    };
    if svg_only {
        let session = document.session.borrow();
        let tree = session.document();
        let mut node = tree.first_child(tree.root()).map_err(dom_error)?;
        let mut svg = false;
        while let Some(current) = node {
            if let NodeKind::Element {
                namespace, name, ..
            } = tree.kind(current).map_err(dom_error)?
            {
                svg = *namespace == Namespace::Svg
                    && lumen_html::xml::split_qname(name.as_str())
                        .is_some_and(|(_, local)| local == "svg");
                break;
            }
            node = tree.next_sibling(current).map_err(dom_error)?;
        }
        if !svg {
            return Ok(Value::Null);
        }
    }
    ctx.with_host_realm(&frame.realm_handle(), |ctx| document.document_value(ctx))
        .map_err(browsing_context::host_realm_error)
}

impl ObjectResources {
    pub(crate) fn pending(&self) -> bool {
        self.load_count.get() != 0
    }
    pub(crate) fn defer_complete(&self) -> bool {
        if !self.pending() {
            return false;
        }
        self.deferred_complete.set(true);
        true
    }
    pub(crate) fn defer_window_load(&self) -> bool {
        if !self.pending() {
            return false;
        }
        self.deferred_load.set(true);
        true
    }
}

impl DomRealm {
    /// Recompute only the changed committed source/style/image epoch, never a
    /// screenshot or used child viewport. No owner Session borrow crosses into a
    /// child Session or layout provider.
    pub(crate) fn refresh_embedded_intrinsic_size(&self,node:NodeId)->OpResult<()> {
        if self.lifecycle.destroyed.get() || self.object_resources.representation(node)!=Representation::Document {return Ok(());}
        let Some(child)=self.embedded_document(node) else { return Ok(()); };
        child.sync_image_bitmaps()?;
        let identity=child.session.borrow().document().root().key();
        let epoch=child.session.borrow_mut().embedded_intrinsic_epoch()
            .map_err(|error|OpError::new("InvalidStateError",format!("embedded intrinsic inputs: {error:?}")))?;
        if self.object_resources.entries.borrow().get(&node).is_some_and(|entry|entry.intrinsic_epoch==Some((identity,epoch))) {return Ok(());}
        let raster=child.content_type.starts_with("image/") && child.content_type!="image/svg+xml";
        let svg=child.session.borrow().embedded_document_is_svg();
        let fonts=if svg {Some(super::canvas::realm_font_source(&child)?)} else {None};
        let natural=child.session.borrow_mut().embedded_document_intrinsic_size(raster,
            fonts.as_ref().map(|fonts|fonts as &dyn lumen_html::paint::TextShaper))
            .map_err(|error|OpError::new("InvalidStateError",format!("embedded intrinsic dimensions: {error:?}")))?;
        self.session.borrow_mut().set_embedded_intrinsic_size(node,natural)
            .map_err(|error|OpError::new("InvalidStateError",format!("embedded intrinsic publication: {error:?}")))?;
        if let Some(entry)=self.object_resources.entries.borrow_mut().get_mut(&node) { entry.intrinsic_epoch=Some((identity,epoch)); }
        Ok(())
    }

    pub(crate) fn refresh_embedded_intrinsic_sizes(&self)->OpResult<()> {
        if !self.object_resources.has_document_representation.get() {return Ok(());}
        let entries=self.object_resources.entries.borrow();
        let count=entries.values().filter(|entry|entry.representation==Representation::Document).count();
        let bytes=count.checked_mul(std::mem::size_of::<NodeId>()).ok_or_else(||OpError::new("QuotaExceededError","embedded size workspace"))?;
        let _reservation=self.object_resources.metadata_budget().reserve(bytes)
            .ok_or_else(||OpError::new("QuotaExceededError","embedded size workspace"))?;
        let mut nodes=Vec::new();
        nodes.try_reserve_exact(count).map_err(|_|OpError::new("QuotaExceededError","embedded size snapshot"))?;
        nodes.extend(entries.iter().filter(|(_,entry)|entry.representation==Representation::Document).map(|(node,_)|*node));
        drop(entries);
        for node in nodes {self.refresh_embedded_intrinsic_size(node)?;}
        Ok(())
    }


    pub fn object_loads_pending(&self) -> bool {
        self.object_resources.pending()
    }
    /// External document installation is a navigation continuation, not I/O
    /// that the interpreter must wait for while returning to its owner loop.
    pub fn object_resources_pending(&self) -> bool {
        self.object_resources
            .entries
            .borrow()
            .values()
            .any(|entry| entry.pending.is_some())
    }

    fn object_selection(self: &Rc<Self>, node: NodeId) -> OpResult<Option<Selection>> {
        if !lumen_html::object::is_embedded(self.session.borrow().document(), node) {
            return Ok(None);
        }
        let rendered = super::style::rendered_ancestry(self, node, true)?;
        let session = self.session.borrow();
        let document = session.document();
        if !lumen_html::object::is_embedded(document, node) {
            return Ok(None);
        }
        let kind = lumen_html::object::kind(document, node).expect("embedded element");
        let entries = self.object_resources.entries.borrow();
        let previous = entries.get(&node);
        let parser_open = entries.get(&node).is_some_and(|entry| entry.parser_open);
        let excluded = lumen_html::object::ancestor_excludes(document, node, |ancestor| {
            entries
                .get(&ancestor)
                .is_none_or(|entry| entry.representation == Representation::Fallback)
        })
        .map_err(dom_error)?;
        let data = document
            .get_attribute_ns_ref(node, None, kind.source_attribute())
            .map_err(dom_error)?;
        let declared_type = document
            .get_attribute_ns_ref(node, None, "type")
            .map_err(dom_error)?
            .unwrap_or("");
        let base = self.base_url();
        let eligible = match kind {
            lumen_html::object::Kind::Object => rendered && !parser_open && !excluded
                && document.get_attribute_ns_ref(node, None, "classid").map_err(dom_error)?.is_none(),
            lumen_html::object::Kind::Embed => {
                let active = self.browsing_context().is_some_and(|context|
                    browsing_context::is_active_document(&context, self));
                active && !excluded && (data.is_some() || document.get_attribute_ns_ref(node, None, "type").map_err(dom_error)?.is_some())
                    && data.is_none_or(|value| !value.is_empty())
                    && (script_loading::is_connected(document,node) || previous.is_some_and(|entry|entry.embed_previous_connected))
                    && (rendered || previous.is_some_and(|entry|entry.embed_previous_rendered))
            }
        };
        if let Some(old) = entries
            .get(&node)
            .and_then(|entry| entry.selection.as_ref())
        {
            if old.data.as_deref() == data
                && old.declared_type.as_ref() == declared_type
                && old.base.as_ref() == base
            {
                let mut selection = old.clone();
                selection.eligible = eligible;
                return Ok(Some(selection));
            }
        }
        let bytes = data
            .map_or(0, str::len)
            .checked_add(declared_type.len())
            .and_then(|bytes| bytes.checked_add(base.len()))
            .and_then(|bytes| bytes.checked_add(256))
            .ok_or_else(|| OpError::new("QuotaExceededError", "object selection metadata size"))?;
        let metadata = self
            .object_resources
            .metadata_budget()
            .reserve(bytes)
            .ok_or_else(|| {
                OpError::new("QuotaExceededError", "object retained request metadata")
            })?;
        Ok(Some(Selection {
            kind,
            data: data.map(Arc::from),
            declared_type: Arc::from(declared_type),
            base: Arc::from(base),
            _metadata: Arc::new(metadata),
            eligible,
        }))
    }

    fn publish_object_representation(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        value: Representation,
    ) -> OpResult<()> {
        let changed = {
            let mut entries = self.object_resources.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&node) else {
                return Ok(());
            };
            let changed = entry.representation != value;
            entry.representation = value;
            entry.intrinsic_epoch = None;
            changed
        };
        if value.has_child_navigable() { self.object_resources.has_document_representation.set(true); }
        if !value.has_child_navigable() {
            self.destroy_object_context(ctx, node);
        }
        self.session
            .borrow_mut()
            .set_object_representation(node, value)
            .map_err(|error| {
                OpError::new(
                    "InvalidStateError",
                    format!("object representation: {error:?}"),
                )
            })?;
        if changed && lumen_html::object::kind(self.session.borrow().document(),node) == Some(lumen_html::object::Kind::Object) {
            // Representation changes, unlike style changes, genuinely alter
            // fallback descendant eligibility and are processed once per change.
            {
                let session = self.session.borrow();
                self.object_resources
                    .dirty_subtree(session.document(), node)
                    .map_err(dom_error)?;
            }
            if let Some(entry) = self.object_resources.entries.borrow_mut().get_mut(&node) {
                entry.dirty = false;
                entry.refetch = false;
            }
            // Admission occurs before the current load lease is released, so
            // fallback descendants also delay the owning document's load.
            self.queue_object_tasks(ctx)?;
        }
        Ok(())
    }

    fn finish_object_load(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        if self.object_resources.pending() || self.lifecycle.destroyed.get() {
            return Ok(());
        }
        if self.object_resources.deferred_complete.replace(false) {
            self.set_document_ready_state(ctx, DocumentReadyState::Complete)?;
        }
        if self.object_resources.deferred_load.replace(false) {
            self.dispatch_window_user_agent(ctx, "load", false, false)?;
        }
        Ok(())
    }
}

impl ObjectResources {
    fn current(&self, node: NodeId, generation: u64) -> bool {
        self.entries
            .borrow()
            .get(&node)
            .is_some_and(|entry| entry.generation == generation && !(entry.dirty && entry.refetch && entry.selection.as_ref().is_some_and(|selection|selection.kind==lumen_html::object::Kind::Embed)))
    }
}

impl ObjectDocumentResponse {
    pub fn into_parts(self) -> (ObjectResponse, String, ObjectDocumentLoad) {
        (
            self.response,
            self.mime,
            ObjectDocumentLoad {
                node: self.node,
                generation: self.generation,
                pending: self.pending,
            },
        )
    }
}

pub struct ObjectDocumentLoad {
    node: NodeId,
    generation: u64,
    pending: Pending,
}
impl ObjectDocumentLoad {
    /// Called by the existing child-document container-load task, not at fetch
    /// completion. Cancellation drops the same document/node load leases.
    pub fn finish(self, ctx: &mut Ctx, owner: &Rc<DomRealm>) -> OpResult<()> {
        let embed={let session=owner.session.borrow();lumen_html::object::kind(session.document(),self.node)==Some(lumen_html::object::Kind::Embed)};
        let eligible = !embed || !owner.lifecycle.destroyed.get() && owner.object_resources.current(self.node,self.generation)
            && owner.object_selection(self.node)?.is_some_and(|selection|selection.eligible);
        let current = eligible && owner.object_resources.current(self.node, self.generation)
            && owner.object_resources.representation(self.node) == Representation::Document;
        if current { owner.refresh_embedded_intrinsic_size(self.node)?; }
        let result = if current && !owner.lifecycle.destroyed.get() {
            owner
                .dispatch_user_agent(ctx, self.node, "load", false, false, &[])
                .map(|_| ())
        } else {
            Ok(())
        };
        drop(self.pending);
        owner.finish_object_load(ctx)?;
        result
    }
}

impl DomRealm {
    pub(crate) fn adopt_object_nodes(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        target: &Rc<Self>,
        mapping: &[(NodeId, NodeId)],
    ) -> OpResult<()> {
        // Requests belong to the old document's settings. Node aliases keep
        // wrapper identity, but a newly inserted owner starts a new algorithm.
        let additional = mapping
            .iter()
            .filter(|(old, new)| {
                self.object_resources.entries.borrow().contains_key(old)
                    && !target.object_resources.entries.borrow().contains_key(new)
            })
            .count();
        {
            let mut entries = target.object_resources.entries.borrow_mut();
            if entries
                .len()
                .checked_add(additional)
                .is_none_or(|count| count > OBJECT_REQUEST_LIMIT)
            {
                return Err(OpError::new(
                    "QuotaExceededError",
                    "object owner adoption limit",
                ));
            }
            entries.try_reserve(additional).map_err(|_| {
                OpError::new("QuotaExceededError", "object owner adoption allocation")
            })?;
        }
        for &(old, new) in mapping {
            let Some(mut entry) = self.object_resources.entries.borrow_mut().remove(&old) else {
                continue;
            };
            target.object_resources.admit(new).map_err(dom_error)?;
            if lumen_html::object::kind(target.session.borrow().document(),new)==Some(lumen_html::object::Kind::Embed) {target.object_resources.has_embeds.set(true);}
            if let (Some(provider), Some(pending)) = (
                self.object_resources.provider.borrow().as_ref(),
                entry.pending.take(),
            ) {
                provider.cancel(pending.ticket);
            }
            entry.document_response = None;
            self.destroy_object_context(ctx, old);
            let mut entries = target.object_resources.entries.borrow_mut();
            let target_entry = entries.get_mut(&new).expect("admitted adopted object");
            target_entry.parser_created |= entry.parser_created;
            target_entry.parser_open |= entry.parser_open;
            target_entry.dirty = true;
            target_entry.refetch = true;
        }
        self.finish_object_load(ctx)
    }

    pub fn take_object_document_response(&self, node: NodeId) -> Option<ObjectDocumentResponse> {
        self.object_resources
            .entries
            .borrow_mut()
            .get_mut(&node)?
            .document_response
            .take()
    }

    /// Queue preparation and consume actual provider completions. A parser
    /// callback only dirties entries; no Session/parser borrow spans a task.
    pub fn queue_object_tasks(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<usize> {
        if self.object_resources.processing_failed.replace(false) {
            return Err(OpError::new(
                "QuotaExceededError",
                "object request admission",
            ));
        }
        if self.lifecycle.destroyed.get() {
            return Ok(0);
        }
        // CSSOM rule changes and environment updates need the same canonical
        // input epoch as style computation, not a private guessed generation.
        if !self.object_resources.entries.borrow().is_empty() {
            let epoch = self
                .session
                .borrow_mut()
                .transition_input_epoch()
                .map_err(|error| {
                    OpError::new(
                        "InvalidStateError",
                        format!("object style inputs: {error:?}"),
                    )
                })?;
            if self.object_resources.style_epoch.replace(Some(epoch)) != Some(epoch) {
                self.object_resources.styles_dirty.set(true);
            }
        }
        if self.object_resources.styles_dirty.replace(false) {
            for entry in self.object_resources.entries.borrow_mut().values_mut() {
                entry.dirty = true;
            }
        }
        let mut nodes = Vec::new();
        {
            let entries = self.object_resources.entries.borrow();
            nodes
                .try_reserve_exact(entries.len())
                .map_err(|_| OpError::new("QuotaExceededError", "object work snapshot"))?;
            nodes.extend(entries.iter().filter_map(|(&node, entry)| {
                (entry.dirty && !entry.task_queued && !entry.parser_open).then_some(node)
            }));
        }
        nodes.sort_unstable_by_key(|node| node.key());
        let mut work = 0;
        for node in nodes {
            if self.session.borrow().document().kind(node).is_err() {
                if let Some(mut entry) = self.object_resources.entries.borrow_mut().remove(&node) {
                    if let (Some(provider), Some(pending)) = (
                        self.object_resources.provider.borrow().as_ref(),
                        entry.pending.take(),
                    ) {
                        provider.cancel(pending.ticket);
                    }
                }
                continue;
            }
            let root = self.retain_resource_request(ctx, node);
            let count = self
                .object_resources
                .load_count
                .get()
                .checked_add(1)
                .ok_or_else(|| OpError::new("QuotaExceededError", "object load counter"))?;
            self.object_resources.load_count.set(count);
            let lease = LoadLease {
                owner: Rc::downgrade(self),
            };
            {
                let mut entries = self.object_resources.entries.borrow_mut();
                let Some(entry) = entries.get_mut(&node) else {
                    continue;
                };
                entry.task_queued = true;
                entry.dirty = false;
            }
            let realm = self.clone();
            scheduling::queue_task(ctx, move |ctx| {
                realm.prepare_object_request(ctx, node, root, lease)
            })?;
            work += 1;
        }
        let provider = self.object_resources.provider.borrow().clone();
        if let Some(provider) = provider {
            let mut pending = Vec::new();
            {
                let entries = self.object_resources.entries.borrow();
                pending.try_reserve_exact(entries.len()).map_err(|_| {
                    OpError::new("QuotaExceededError", "object completion snapshot")
                })?;
                pending.extend(entries.iter().filter_map(|(&node, entry)| {
                    entry
                        .pending
                        .as_ref()
                        .map(|pending| (node, pending.ticket, pending.generation))
                }));
            }
            for (node, ticket, generation) in pending {
                let Some(result) = provider.poll(ticket) else {
                    continue;
                };
                let lease = self
                    .object_resources
                    .entries
                    .borrow_mut()
                    .get_mut(&node)
                    .filter(|entry| entry.generation == generation)
                    .and_then(|entry| entry.pending.take());
                let Some(lease) = lease else {
                    continue;
                };
                let realm = self.clone();
                scheduling::queue_task(ctx, move |ctx| {
                    realm.complete_object_request(ctx, node, generation, lease, result)
                })?;
                work += 1;
            }
        }
        self.finish_object_load(ctx)?;
        Ok(work)
    }

    fn prepare_object_request(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        root: ResourceRequestRoot,
        lease: LoadLease,
    ) -> OpResult<()> {
        if let Some(entry) = self.object_resources.entries.borrow_mut().get_mut(&node) {
            entry.task_queued = false;
        }
        if self.lifecycle.destroyed.get() {
            return Ok(());
        }
        let Some(selected) = self.object_selection(node)? else {
            return Ok(());
        };
        let (generation, old) = {
            let mut entries = self.object_resources.entries.borrow_mut();
            let Some(entry) = entries.get_mut(&node) else {
                return Ok(());
            };
            let refetch = core::mem::take(&mut entry.refetch);
            if !refetch
                && entry.selection.as_ref().is_some_and(|old| {
                    old.eligible == selected.eligible
                        && (selected.kind == lumen_html::object::Kind::Embed || old.data == selected.data && old.base == selected.base)
                })
            {
                drop(entries);
                drop(lease);
                return self.finish_object_load(ctx);
            }
            entry.generation = entry
                .generation
                .checked_add(1)
                .ok_or_else(|| OpError::new("QuotaExceededError", "object generation exhausted"))?;
            entry.selection = Some(selected.clone());
            entry.document_response = None;
            (entry.generation, entry.pending.take())
        };
        let provider = self.object_resources.provider.borrow().clone();
        if let (Some(provider), Some(old)) = (&provider, old) {
            provider.cancel(old.ticket);
        }
        if !selected.eligible || selected.data.as_deref().is_none_or(str::is_empty) {
            self.publish_object_representation(ctx, node, selected.kind.inactive())?;
            drop(lease);
            return self.finish_object_load(ctx);
        }
        let url = match lumen_common::url::parse(
            selected.data.as_deref().unwrap_or(""),
            Some(&selected.base),
        ) {
            Ok(url) => url.href(),
            Err(_) => {
                if selected.kind == lumen_html::object::Kind::Embed {
                    drop((root, lease));
                    return self.finish_object_load(ctx);
                }
                return self.object_request_failed(ctx, node, (root, lease));
            }
        };
        let parser_inserted = self
            .object_resources
            .entries
            .borrow()
            .get(&node)
            .is_some_and(|entry| entry.parser_created);
        let local_resource =
            if lumen_common::url::parse(&url, None).is_ok_and(|url| url.scheme == "blob") {
                let origin = self
                    .browsing_context()
                    .map(|context| context.root_or_child_origin());
                origin.and_then(|origin| object_urls::get_for_origin(ctx, &url, &origin))
            } else {
                None
            };
        if local_resource
            .as_ref()
            .is_some_and(|resource| resource.bytes.len() > OBJECT_BODY_LIMIT)
        {
            return Err(OpError::new(
                "QuotaExceededError",
                "object Blob resource bytes",
            ));
        }
        let document_url = self.document_url().unwrap_or_default();
        let origin = self.script_fetch_origin();
        let referrer = self.navigation_referrer();
        let bytes = url
            .len()
            .checked_add(document_url.len())
            .and_then(|bytes| bytes.checked_add(origin.len()))
            .and_then(|bytes| bytes.checked_add(referrer.source.len()))
            .and_then(|bytes| bytes.checked_add(selected.declared_type.len()))
            .and_then(|bytes| {
                bytes.checked_add(
                    local_resource
                        .as_ref()
                        .map_or(0, |resource| resource.bytes.len()),
                )
            })
            .and_then(|bytes| bytes.checked_add(256))
            .ok_or_else(|| OpError::new("QuotaExceededError", "object request metadata size"))?;
        let metadata = self
            .object_resources
            .metadata_budget()
            .reserve(bytes)
            .ok_or_else(|| {
                OpError::new("QuotaExceededError", "object retained request metadata")
            })?;
        let metadata = Arc::new(metadata);
        // Reserve the retained payload before copying the realm-local blob into
        // the transport's thread-safe storage. The request owns this lease.
        let local_resource = local_resource.map(|resource| ObjectLocalResource {
            creator_origin: object_urls::resource_origin(&resource),
            bytes: lumen_common::bytes::Bytes::owned(Arc::from(&*resource.bytes)),
            mime: resource.content_type,
            _source_reservation: metadata.clone(),
        });
        let request = ObjectRequest {
            url,
            destination: match selected.kind { lumen_html::object::Kind::Object => lumen_common::csp::Destination::Object, lumen_html::object::Kind::Embed => lumen_common::csp::Destination::Embed },
            document_url,
            origin,
            referrer,
            policies: self.module_fetch_policy_snapshot()?,
            parser_inserted,
            declared_type: selected.declared_type.to_string(),
            local_resource,
            metadata_reservation: metadata,
        };
        let Some(provider) = provider else {
            return Err(OpError::new(
                "NotSupportedError",
                "object resource transport unavailable",
            ));
        };
        match provider.start(request) {
            Ok(ticket) => {
                let pending = Pending {
                    ticket,
                    generation,
                    _target: root,
                    _load: lease,
                };
                if let Some(result) = provider.poll(ticket) {
                    return self.complete_object_request(ctx, node, generation, pending, result);
                }
                // Unavailable resources use fallback until their networking
                // continuation restarts the response-processing phase.
                self.publish_object_representation(ctx, node, selected.kind.inactive())?;
                let mut entries = self.object_resources.entries.borrow_mut();
                if let Some(entry) = entries
                    .get_mut(&node)
                    .filter(|entry| entry.generation == generation)
                {
                    entry.pending = Some(pending);
                } else {
                    provider.cancel(ticket);
                }
                Ok(())
            }
            Err(_) => self.object_request_failed(ctx, node, (root, lease)),
        }
    }

    fn complete_object_request(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        generation: u64,
        lease: Pending,
        result: Result<ObjectResponse, ObjectFailure>,
    ) -> OpResult<()> {
        if self.lifecycle.destroyed.get()
            || !self.object_resources.current(node, generation)
            || self.session.borrow().document().kind(node).is_err()
        {
            drop(lease);
            return self.finish_object_load(ctx);
        }
        let embed={let session=self.session.borrow();lumen_html::object::kind(session.document(),node)==Some(lumen_html::object::Kind::Embed)};
        if embed && self.object_selection(node)?.is_none_or(|selection|!selection.eligible) {
            self.publish_object_representation(ctx,node,Representation::Nothing)?;
            drop(lease);
            return self.finish_object_load(ctx);
        }
        let mut response = match result {
            Ok(response) => response,
            Err(failure) => {
                self.report_module_fetch_violations(ctx, failure.violations)?;
                return self.object_request_failed(ctx, node, lease);
            }
        };
        self.report_module_fetch_violations(ctx, core::mem::take(&mut response.violations))?;
        let kind = lumen_html::object::kind(self.session.borrow().document(), node).expect("current embedded element");
        if kind == lumen_html::object::Kind::Object && response.response.status >= 400 {
            return self.object_request_failed(ctx, node, lease);
        }
        let declared = self
            .object_resources
            .entries
            .borrow()
            .get(&node)
            .and_then(|entry| entry.selection.as_ref())
            .map(|selection| selection.declared_type.clone())
            .unwrap_or_default();
        let mime = lumen_common::mime::extract_mime_type(
            response
                .response
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-type"))
                .map(|(_, value)| value.as_str()),
        );
        let mime = if kind == lumen_html::object::Kind::Embed {
            // Native document/image handlers are the UA's supported integrations.
            // Unknown plug-in types never acquire a fabricated representation.
            let supported = |mime: &str| mime == "text/html" || lumen_common::mime::is_xml_mime(Some(mime))
                || lumen_common::mime::is_text_document_mime(Some(mime))
                || matches!(mime,"image/png"|"image/jpeg"|"image/gif"|"image/webp"|"image/bmp"|"image/x-ms-bmp");
            let declared = lumen_common::mime::extract_mime_type(std::iter::once(declared.as_ref()));
            declared.filter(|value|supported(value)).or_else(||mime.filter(|value|supported(value)))
                .unwrap_or_default()
        } else {
            lumen_common::mime::object_resource_mime(mime.as_deref(),Some(&declared),&response.response.body)
        };
        // XML (including SVG) has precedence over the image category.
        if kind == lumen_html::object::Kind::Embed && !mime.is_empty()
            || lumen_common::mime::is_xml_mime(Some(&mime))
            || mime == "text/html"
            || lumen_common::mime::is_text_document_mime(Some(&mime))
        {
            let had_context = self.represented_object_context(node).is_some();
            self.publish_object_representation(ctx, node, Representation::Document)?;
            let frame = self.ensure_frame_context(ctx, node)?;
            // Embed setup navigates its existing content navigable even for
            // about:blank; only Object keeps the existing blank document.
            if !lumen_common::url::parse(&response.response.url, None)
                .is_ok_and(|url| url.scheme == "about" && url.path == "blank")
                || had_context && kind == lumen_html::object::Kind::Embed
            {
                frame.request_object_response(&response.response.url, self)?;
            } else if had_context && kind == lumen_html::object::Kind::Object {
                // about:blank does not navigate an existing content navigable.
                drop(lease);
                return self.finish_object_load(ctx);
            }
            if let Some(entry) = self.object_resources.entries.borrow_mut().get_mut(&node) {
                entry.document_response = Some(ObjectDocumentResponse {
                    node,
                    generation,
                    response,
                    mime,
                    pending: lease,
                });
            }
            return Ok(());
        }
        if mime.starts_with("image/") {
            let images = self.object_resources.images.borrow().clone();
            let remaining = {
                let mut images = images.borrow_mut();
                images.retain(|image| image.strong_count() != 0);
                let mut bytes = 0usize;
                for image in images.iter().filter_map(std::sync::Weak::upgrade) {
                    bytes = bytes.checked_add(image.pixels.len()).ok_or_else(|| {
                        OpError::new("QuotaExceededError", "object decoded image bytes")
                    })?;
                }
                if images.len() >= OBJECT_REQUEST_LIMIT {
                    return Err(OpError::new(
                        "QuotaExceededError",
                        "object image reservations",
                    ));
                }
                images.try_reserve(1).map_err(|_| {
                    OpError::new("QuotaExceededError", "object image reservation allocation")
                })?;
                (32usize * 1024 * 1024).checked_sub(bytes).ok_or_else(|| {
                    OpError::new("QuotaExceededError", "object decoded image budget")
                })?
            };
            if let Ok(decoded) =
                lumen_html_image::decode_raster_image_with_limit(&response.response.body, remaining)
            {
                let image = Arc::new(lumen_html::paint::ImageData {
                    width: decoded.width,
                    height: decoded.height,
                    pixels: decoded.pixels,
                });
                images.borrow_mut().push(Arc::downgrade(&image));
                self.publish_object_representation(ctx, node, Representation::Image)?;
                self.session
                    .borrow_mut()
                    .set_node_bitmap(node, Some(image))
                    .map_err(|error| {
                        OpError::new("InvalidStateError", format!("object bitmap: {error:?}"))
                    })?;
                return self.queue_object_terminal_event(ctx, node, generation, "load", lease);
            }
        }
        self.publish_object_representation(ctx, node, kind.inactive())?;
        drop(lease);
        self.finish_object_load(ctx)
    }
}

impl DomRealm {
    fn object_request_failed<P>(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        payload: P,
    ) -> OpResult<()> {
        let kind = lumen_html::object::kind(self.session.borrow().document(), node);
        let event = self.dispatch_user_agent(ctx, node, if kind == Some(lumen_html::object::Kind::Embed) {"load"} else {"error"}, false, false, &[]);
        // Embed network errors leave the previous representation intact. Object
        // errors fire before exposing fallback and destroying the old navigable.
        if kind != Some(lumen_html::object::Kind::Embed) {
            self.publish_object_representation(ctx, node, Representation::Fallback)?;
        }
        drop(payload);
        self.finish_object_load(ctx)?;
        event.map(|_| ())
    }

    fn queue_object_terminal_event<P: 'static>(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        generation: u64,
        kind: &'static str,
        payload: P,
    ) -> OpResult<()> {
        let realm = self.clone();
        scheduling::queue_task(ctx, move |ctx| {
            let result = if !realm.lifecycle.destroyed.get()
                && realm.object_resources.current(node, generation)
                && realm.session.borrow().document().kind(node).is_ok()
            {
                realm
                    .dispatch_user_agent(ctx, node, kind, false, false, &[])
                    .map(|_| ())
            } else {
                Ok(())
            };
            drop(payload);
            realm.finish_object_load(ctx)?;
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct Loader {
        next: Cell<u64>,
        requests: RefCell<Vec<(u64, ObjectRequest)>>,
        replies: RefCell<HashMap<u64, Result<ObjectResponse, ObjectFailure>>>,
        cancelled: RefCell<Vec<u64>>,
        fail_next: Cell<bool>,
    }
    impl ObjectResourceLoader for Loader {
        fn start(&self, request: ObjectRequest) -> Result<u64, String> {
            if self.fail_next.replace(false) {
                return Err("controlled transport failure".into());
            }
            let id = self.next.get() + 1;
            self.next.set(id);
            self.requests.borrow_mut().push((id, request));
            Ok(id)
        }
        fn poll(&self, id: u64) -> Option<Result<ObjectResponse, ObjectFailure>> {
            self.replies.borrow_mut().remove(&id)
        }
        fn cancel(&self, id: u64) {
            self.cancelled.borrow_mut().push(id);
            self.replies.borrow_mut().remove(&id);
        }
    }
    fn environment(engine: &mut lumen::Engine) -> Rc<Loader> {
        let loader = Rc::new(Loader::default());
        let provider = loader.clone();
        engine.ctx().op_state().put(ObjectEnvironment::new(
            move |_| provider.clone(),
            Rc::new(RefCell::new(Vec::new())),
        ));
        loader
    }
    fn pump(engine: &mut lumen::Engine, realm: &Rc<DomRealm>) {
        realm
            .queue_object_tasks(engine.ctx())
            .expect("queue actual object work");
        let errors = scheduling::run_tasks(engine, 64);
        if let Some(error) = errors.first() {
            let text = engine
                .ctx()
                .coerce_string(error)
                .ok()
                .map(|s| s.to_string())
                .unwrap_or_default();
            panic!("object task: {text}");
        }
    }
    fn check(engine: &mut lumen::Engine, source: &str) {
        match engine
            .eval_value(source)
            .expect("compile native object guard")
        {
            Ok(Value::Bool(true)) => {}
            Ok(_) => panic!("object guard returned false: {source}"),
            Err(error) => {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .ok()
                    .map(|s| s.to_string())
                    .unwrap_or_default();
                panic!("object guard threw: {message}");
            }
        }
    }
    fn response(body: Vec<u8>, mime: &str, url: &str) -> ObjectResponse {
        let reservation = lumen_common::limits::ByteBudget::new(body.len())
            .reserve(body.len())
            .expect("actual response bytes");
        ObjectResponse {
            response: lumen_common::http_body::SyncHttpResponse {
                status: 200,
                status_text: "OK".into(),
                url: url.into(),
                headers: vec![("Content-Type".into(), mime.into())],
                body,
            },
            request_referrer: None,
            creator_origin: None,
            reservation,
            violations: Vec::new(),
        }
    }
    #[test]
    fn specification_embed_eligibility_reflection_source_supersession_and_network_error_phases() {
        let mut engine=lumen::Engine::new();
        let loader=environment(&mut engine);
        let realm=crate::install(engine.ctx(),"<!doctype html><embed id=empty><embed id=typeOnly type='application/unknown'><embed id=hidden style='display:none' src='/hidden'><video><embed src='/media'></video><object data='/outer'><embed src='/fallback'></object><embed id=e src='/first'>",256).unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine,"globalThis.embedEvents=[];e.onload=event=>embedEvents.push([event.type,event.isTrusted]);e.onerror=()=>embedEvents.push(['error']);e.name='actualName';e.align='left';e.width='40';e.height='20';e.type='TEXT/HTML';e.src='/latest';e.src.endsWith('/latest') && e.name==='actualName' && e.align==='left' && e.getSVGDocument()===null && !('contentDocument' in HTMLEmbedElement.prototype)");
        check(&mut engine,r#"(() => {
        const descriptor=Object.getOwnPropertyDescriptor(HTMLEmbedElement.prototype,'src');
        const original=e.getAttribute('src'),failure={};
        try {e.src={toString(){throw failure}};throw Error('missing conversion failure')} catch(error){if(error!==failure)throw error}
        try {e.src=Symbol();throw Error('accepted Symbol')} catch(error){if(!(error instanceof TypeError))throw error}
        return e.getAttribute('src')===original && descriptor.get.length===0 && descriptor.set.length===1;
    })()"#);
    pump(&mut engine,&realm);
        let requests=loader.requests.borrow();
        assert!(requests.iter().all(|(_,request)|!request.url.ends_with("/hidden") && !request.url.ends_with("/media") && !request.url.ends_with("/first")));
        let ticket=requests.iter().find(|(_,request)|request.url.ends_with("/latest")).expect("latest embed source").0;
        assert_eq!(requests.iter().find(|(id,_)|*id==ticket).unwrap().1.destination,lumen_common::csp::Destination::Embed);
        drop(requests);
        check(&mut engine,"e.src='/successor';true");
        pump(&mut engine,&realm);
        assert!(loader.cancelled.borrow().contains(&ticket));
        let successor=loader.requests.borrow().iter().find(|(_,request)|request.url.ends_with("/successor")).unwrap().0;
        loader.replies.borrow_mut().insert(successor,Err(ObjectFailure{message:"network failure".into(),violations:Vec::new()}));
        pump(&mut engine,&realm);
        check(&mut engine,"embedEvents.length===1 && embedEvents[0].join(',')==='load,true' && e.getSVGDocument()===null");
        check(&mut engine,"e.src='http://[';true");
        pump(&mut engine,&realm);
        check(&mut engine,"embedEvents.length===1");
        realm.object_resources.retire();
    }

    #[test]
    fn specification_embed_last_turn_eligibility_cancels_at_next_turn_and_adoption_starts_new_owner() {
        let mut engine=lumen::Engine::new();
        let loader=environment(&mut engine);
        let realm=crate::install(engine.ctx(),"<!doctype html><embed id=e src='/first'>",128).unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine,"globalThis.retainedEmbed=e;globalThis.embedEvents=[];e.onload=()=>embedEvents.push('load');e.onerror=()=>embedEvents.push('error');true");
        pump(&mut engine,&realm);
        let first=loader.requests.borrow()[0].0;
        check(&mut engine,"retainedEmbed.remove();true");
        realm.queue_object_tasks(engine.ctx()).unwrap();
        assert!(realm.object_loads_pending(),"previous-turn connected/rendered ownership lasts until the next event-loop turn");
        assert!(!loader.cancelled.borrow().contains(&first));
        pump(&mut engine,&realm);
        assert!(loader.cancelled.borrow().contains(&first));
        assert!(!realm.object_loads_pending());
        check(&mut engine,"document.body.append(retainedEmbed);true");
        pump(&mut engine,&realm);
        let second=loader.requests.borrow().last().unwrap().0;
        assert_ne!(first,second,"becoming potentially active queues a new actual source request");
        check(&mut engine,"globalThis.inertEmbedOwner=document.implementation.createHTMLDocument('inert');inertEmbedOwner.adoptNode(retainedEmbed);inertEmbedOwner.body.append(retainedEmbed);retainedEmbed.ownerDocument===inertEmbedOwner && retainedEmbed.getSVGDocument()===null");
        assert!(loader.cancelled.borrow().contains(&second));
        pump(&mut engine,&realm);
        assert!(!realm.object_resources_pending());
        check(&mut engine,"embedEvents.length===0");
        realm.object_resources.retire();
    }

    #[test]
    fn specification_embed_supported_responses_create_real_navigables_non_ok_status_and_nothing_teardown() {
        let mut engine=lumen::Engine::new();
        let loader=environment(&mut engine);
        let realm=crate::install(engine.ctx(),"<!doctype html><embed id=e src='/document'>",128).unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine,"globalThis.embedEvents=[];e.onload=event=>embedEvents.push(event.type);true");
        pump(&mut engine,&realm);
        let ticket=loader.requests.borrow()[0].0;
        let mut supplied=response(b"<!doctype html><p>actual response</p>".to_vec(),"text/html","https://web-platform.test/document");
        supplied.response.status=500;
        loader.replies.borrow_mut().insert(ticket,Ok(supplied));
        pump(&mut engine,&realm);
        let frames=realm.frame_contexts(engine.ctx()).unwrap();
        assert_eq!(frames.len(),1,"supported response uses the shared real child-navigable controller");
        let frame=&frames[0];
        let captured=realm.take_object_document_response(frame.owner_node()).expect("actual response continuation");
        assert_eq!(captured.mime,"text/html");
        assert_eq!(captured.response.response.status,500,"embed must not reject non-ok plugin/document payloads");
        assert_eq!(captured.response.response.body,b"<!doctype html><p>actual response</p>");
        check(&mut engine,"embedEvents.length===0 && frames.length===1");
        drop(captured);
        check(&mut engine,"e.src='/unsupported';e.removeAttribute('type');true");
        pump(&mut engine,&realm);
        let ticket=loader.requests.borrow().last().unwrap().0;
        loader.replies.borrow_mut().insert(ticket,Ok(response(Vec::new(),"application/unknown","https://web-platform.test/unsupported")));
        pump(&mut engine,&realm);
        assert!(realm.frame_contexts(engine.ctx()).unwrap().is_empty());
        assert!(realm.object_resources.entries.borrow().values().all(|entry|entry.representation==Representation::Nothing));
        check(&mut engine,"embedEvents.length===0 && e.getSVGDocument()===null && frames.length===0");
        realm.object_resources.retire();
    }
    #[test]
    fn specification_embed_committed_svg_intrinsic_identity_precedes_load_and_tracks_child_changes() {
        let mut engine=lumen::Engine::new();
        let loader=environment(&mut engine);
        let owner=crate::install(engine.ctx(),"<!doctype html><embed id=e src='/actual.svg'>",128).unwrap();
        owner.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine,"globalThis.embedLoads=[];e.onload=()=>embedLoads.push(e.getSVGDocument().documentElement.getAttribute('width'));true");
        pump(&mut engine,&owner);
        let ticket=loader.requests.borrow()[0].0;
        loader.replies.borrow_mut().insert(ticket,Ok(response(b"<svg xmlns='http://www.w3.org/2000/svg' width='100' height='80'/>".to_vec(),"image/svg+xml","https://web-platform.test/actual.svg")));
        pump(&mut engine,&owner);
        let frame=owner.frame_contexts(engine.ctx()).unwrap().remove(0);
        let continuation=owner.take_object_document_response(frame.owner_node()).unwrap();
        let request=frame.navigation_request();
        let child=frame.install_response_for_request(engine.ctx(),&request,"https://web-platform.test/actual.svg","image/svg+xml","<svg xmlns='http://www.w3.org/2000/svg' width='100' height='80'/>",128).unwrap();
        let (_,_,completion)=continuation.into_parts();
    completion.finish(engine.ctx(),&owner).unwrap();
        check(&mut engine,"embedLoads.join(',')==='100' && e.getSVGDocument()!==null");
        let root=child.session.borrow().document().document_element_at(child.session.borrow().document().root()).unwrap().unwrap();
        child.session.borrow_mut().document_mut().set_attribute(root,"width","40").unwrap();
        child.session.borrow_mut().document_mut().set_attribute(root,"height","20").unwrap();
        owner.refresh_embedded_intrinsic_size(frame.owner_node()).unwrap();
        let expected=child.session.borrow_mut().embedded_document_intrinsic_size(false,None).unwrap().unwrap();
        assert_eq!(expected.width,Some(40.0));
        assert_eq!(expected.height,Some(20.0));
        assert_eq!(expected.ratio,Some(2.0));
        owner.object_resources.retire();
    }

    #[test]
    fn specification_object_parser_completion_media_fallback_and_image_task_phases() {
        let mut engine = lumen::Engine::new();
        let loader = environment(&mut engine);
        let realm=crate::install_live_html(engine.ctx(),r#"<!doctype html><object id=outer data='/outer'><object id=inner data='/inner'></object><script>0</script></object><video><object data='/ignored'></object></video>"#,256).unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        assert!(realm
            .next_document_parser_script(engine.ctx())
            .unwrap()
            .is_some());
        pump(&mut engine, &realm);
        assert_eq!(
            loader.requests.borrow().len(),
            1,
            "open outer parser element stays fallback; inner has completed"
        );
        assert!(loader.requests.borrow()[0].1.url.ends_with("/inner"));
        assert!(realm
            .next_document_parser_script(engine.ctx())
            .unwrap()
            .is_none());
        check(&mut engine,"globalThis.objectLoads=[];outer.onload=()=>objectLoads.push('outer');inner.onload=()=>objectLoads.push('inner');outer.contentDocument===null && inner.contentDocument===null");
        pump(&mut engine, &realm);
        assert_eq!(
            loader.requests.borrow().len(),
            2,
            "EOF schedules outer; media descendant never fetches"
        );
        let inner = loader.requests.borrow()[0].0;
        let outer = loader.requests.borrow()[1].0;
        let png = lumen_html_image::encode_png(&lumen_html_image::Rgba8Image {
            width: 2,
            height: 1,
            pixels: vec![0, 128, 0, 255, 0, 128, 0, 255],
        });
        loader.replies.borrow_mut().insert(
            outer,
            Ok(response(
                png,
                "image/png",
                "https://web-platform.test/outer",
            )),
        );
        pump(&mut engine, &realm);
        check(&mut engine,"objectLoads.length===0 && outer.contentWindow===null && inner.contentWindow===null");
        assert!(realm.object_resources.entries.borrow().values().any(|entry|entry.representation==Representation::Image),
            "response processing publishes the decoded representation before its queued load event");
        pump(&mut engine, &realm);
        check(&mut engine,"(() => {const state=[objectLoads.join(','),outer.contentWindow===null,inner.contentWindow===null];if(state[0]==='outer' && state[1] && state[2])return true;throw new Error('object image phase: '+JSON.stringify(state));})()");
        pump(&mut engine, &realm);
        assert!(
            loader.cancelled.borrow().contains(&inner),
            "image suppresses and cancels fallback descendant request"
        );
        assert!(!realm.object_loads_pending());
    }
    #[test]
    fn specification_object_error_before_fallback_source_cancellation_and_adoption() {
        let mut engine = lumen::Engine::new();
        let loader = environment(&mut engine);
        let realm = crate::install(
            engine.ctx(),
            "<!doctype html><object id=o data='/first'>fallback</object>",
            256,
        )
        .unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine,"globalThis.events=[];o.onerror=e=>events.push([e.type,e.isTrusted,o.contentDocument===null]);o.data.endsWith('/first') && o.type===''");
        check(
            &mut engine,
            r#"(() => {
            const descriptor=Object.getOwnPropertyDescriptor(HTMLObjectElement.prototype,'data');
            const original=o.getAttribute('data');const failure={};
            try{o.data={toString(){throw failure}};throw Error('missing conversion throw')}catch(error){if(error!==failure)throw error;}
            try{o.data=Symbol();throw Error('accepted Symbol')}catch(error){if(!(error instanceof TypeError))throw error;}
            return o.getAttribute('data')===original && descriptor.get.length===0 && descriptor.set.length===1;
        })()"#,
        );
        pump(&mut engine, &realm);
        let first = loader.requests.borrow()[0].0;
        check(&mut engine,"o.type='text/html';o.data='/second';o.width='40';o.height='20';o.getAttribute('data')==='/second' && o.width==='40'");
        pump(&mut engine, &realm);
        assert!(loader.cancelled.borrow().contains(&first));
        let second = loader.requests.borrow().last().unwrap().0;
        loader.replies.borrow_mut().insert(
            first,
            Ok(response(
                Vec::new(),
                "text/html",
                "https://web-platform.test/first",
            )),
        );
        loader.replies.borrow_mut().insert(
            second,
            Err(ObjectFailure {
                message: "network failed".into(),
                violations: Vec::new(),
            }),
        );
        pump(&mut engine, &realm);
        check(&mut engine,"events.length===1 && events[0].join(',')==='error,true,true' && o.contentDocument===null && o.textContent==='fallback'");
        check(&mut engine, "o.data='/third';true");
        pump(&mut engine, &realm);
        let third = loader.requests.borrow().last().unwrap().0;
        check(&mut engine,"globalThis.retainedAdoptedObject=o;globalThis.inert=document.implementation.createHTMLDocument('inert');inert.adoptNode(retainedAdoptedObject);inert.body.append(retainedAdoptedObject);retainedAdoptedObject.ownerDocument===inert && retainedAdoptedObject.contentWindow===null");
        assert!(loader.cancelled.borrow().contains(&third));
        pump(&mut engine, &realm);
        assert!(!realm.object_resources_pending());
        realm.object_resources.retire();
    }
    #[test]
    fn specification_object_error_precedes_document_teardown_and_cancels_ready_source() {
        let mut engine = lumen::Engine::new();
        let loader = environment(&mut engine);
        let realm = crate::install(
            engine.ctx(),
            "<!doctype html><object id=o data='/document'>fallback</object>",
            128,
        )
        .unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        pump(&mut engine, &realm);
        let ticket = loader.requests.borrow()[0].0;
        loader.replies.borrow_mut().insert(
            ticket,
            Ok(response(
                b"<!doctype html><body>actual source".to_vec(),
                "text/html",
                "https://web-platform.test/document",
            )),
        );
        pump(&mut engine, &realm);
        check(&mut engine,"globalThis.savedObjectDocument=o.contentDocument;globalThis.errorBeforeFallback=false;o.onerror=e=>{errorBeforeFallback=e.isTrusted && e.target===o && o.contentDocument===savedObjectDocument;};savedObjectDocument!==null");
        assert!(
            realm.object_loads_pending(),
            "real document response awaits the shared host controller"
        );
        loader.fail_next.set(true);
        check(&mut engine, "o.data='/failed';true");
        pump(&mut engine, &realm);
        check(&mut engine,"errorBeforeFallback && o.contentDocument===null && o.contentWindow===null && o.textContent==='fallback'");
        assert!(
            !realm.object_loads_pending(),
            "failed replacement releases the staged document source and load leases"
        );
        assert!(realm.frame_contexts(engine.ctx()).unwrap().is_empty());
    }
    #[test]
    fn specification_object_blob_capture_charges_actual_source_until_request_reclamation() {
        let mut engine = lumen::Engine::new();
        assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());
        assert!(lumen_host::lazy_globals::<lumen_host::url::bindings::Module>(engine.ctx()).is_ok());
        let loader = environment(&mut engine);
        let realm = crate::install(
            engine.ctx(),
            "<!doctype html><object id=o>fallback</object>",
            128,
        )
        .unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine, "o.data=URL.createObjectURL(new Blob(['actual captured source'],{type:'text/html'}));true");
        pump(&mut engine, &realm);
        let (_, request) = loader
            .requests
            .borrow_mut()
            .pop()
            .expect("actual captured Blob request");
        let resource = request
            .local_resource
            .as_ref()
            .expect("partition-qualified local Blob");
        assert_eq!(&*resource.bytes, b"actual captured source");
        let retained_bytes = request.metadata_reservation.bytes();
        assert!(
            retained_bytes >= resource.bytes.len(),
            "source payload is admitted alongside request metadata before its transport copy"
        );
        let budget = realm.object_resources.metadata_budget();
        let before = budget.reserved();
        let retained_resource = resource.clone();
        drop(request);
        assert_eq!(
            budget.reserved(),
            before,
            "an actual retained source keeps its bytes admitted"
        );
        drop(retained_resource);
        assert_eq!(
            budget.reserved(),
            before - retained_bytes,
            "actual request reclamation releases its source envelope"
        );
        realm.object_resources.retire();
    }

    #[test]
    fn specification_object_empty_data_uses_fallback_while_nonempty_url_is_parsed() {
        let mut engine = lumen::Engine::new();
        let loader = environment(&mut engine);
        let realm = crate::install(
            engine.ctx(),
            "<!doctype html><object id=o data=''>actual fallback</object>",
            128,
        )
        .unwrap();
        realm.set_document_url("https://web-platform.test/parent.html");
        check(&mut engine, "globalThis.objectEvents=[];o.onerror=e=>objectEvents.push(e.type);o.onload=e=>objectEvents.push(e.type);true");
        pump(&mut engine, &realm);
        assert!(
            loader.requests.borrow().is_empty(),
            "empty object data does not fetch the owner URL"
        );
        assert!(!realm.object_loads_pending());
        check(&mut engine, "o.contentDocument===null && o.contentWindow===null && o.textContent==='actual fallback' && objectEvents.length===0");
        check(&mut engine, "o.data=' ';true");
        pump(&mut engine, &realm);
        assert_eq!(
            loader.requests.borrow().len(),
            1,
            "nonempty data reaches the real URL parser"
        );
        assert_eq!(
            loader.requests.borrow()[0].1.url,
            "https://web-platform.test/parent.html"
        );
        check(&mut engine, "o.data='';true");
        pump(&mut engine, &realm);
        assert_eq!(
            loader.cancelled.borrow().len(),
            1,
            "fallback cancels the previous settings-owned request"
        );
        assert!(!realm.object_loads_pending());
        check(
            &mut engine,
            "o.contentDocument===null && objectEvents.length===0",
        );
        realm.object_resources.retire();
    }
}

/// Sample the two embed history conditions once at the host's next event-loop
/// turn, not at parser/microtask checkpoints. The existing environment contains
/// weak document owners, so this adds no browsing-context lifetime root.
pub(crate) fn checkpoint_embed_tasks(engine: &mut lumen::Engine) -> Vec<Value> {
    let environment = engine.ctx().op_state().get::<ObjectEnvironment>().map(|environment|(environment.documents.clone(),environment.metadata.clone()));
    let Some((documents,metadata)) = environment else {return Vec::new()};
    let owner_count=documents.borrow().iter().filter_map(std::rc::Weak::upgrade).filter(|owner|(owner.object_resources.has_embeds.get() || owner.object_resources.has_document_representation.get()) && !owner.lifecycle.destroyed.get()).count();
    if owner_count==0 {return Vec::new()};
    let owner_bytes=owner_count.checked_mul(std::mem::size_of::<Rc<DomRealm>>());
    let Some(_owners_reservation)=owner_bytes.and_then(|bytes|metadata.reserve(bytes)) else {return vec![engine.ctx().make_error("QuotaExceededError","embedded owner turn workspace")]};
    let mut owners=Vec::new();
    {
        let mut documents=documents.borrow_mut();
        documents.retain(|document|document.strong_count()!=0);
        if owners.try_reserve_exact(owner_count).is_err() {
            return vec![engine.ctx().make_error("QuotaExceededError","embedded document turn snapshot")];
        }
        owners.extend(documents.iter().filter_map(std::rc::Weak::upgrade).filter(|owner|(owner.object_resources.has_embeds.get() || owner.object_resources.has_document_representation.get()) && !owner.lifecycle.destroyed.get()));
    }
    let mut errors=Vec::new();
    for owner in owners {
        if owner.lifecycle.destroyed.get() {continue;}
        let result=(|| -> OpResult<()> {
            let mut nodes=Vec::new();
            let _node_reservation;
            {
                let session=owner.session.borrow();
                let entries=owner.object_resources.entries.borrow();
                let bytes=entries.len().checked_mul(std::mem::size_of::<NodeId>()).ok_or_else(||OpError::new("QuotaExceededError","embed turn workspace size"))?;
                _node_reservation=metadata.reserve(bytes).ok_or_else(||OpError::new("QuotaExceededError","embed turn workspace"))?;
                nodes.try_reserve_exact(entries.len()).map_err(|_|OpError::new("QuotaExceededError","embed turn snapshot"))?;
                nodes.extend(entries.keys().copied().filter(|node|lumen_html::object::kind(session.document(),*node)==Some(lumen_html::object::Kind::Embed)));
            }
            owner.refresh_embedded_intrinsic_sizes()?;
            if nodes.is_empty() {return Ok(());}
            for node in nodes {
                let connected=script_loading::is_connected(owner.session.borrow().document(),node);
                let rendered=super::style::rendered_ancestry(&owner,node,true)?;
                if let Some(entry)=owner.object_resources.entries.borrow_mut().get_mut(&node) {
                    if entry.embed_previous_connected!=connected || entry.embed_previous_rendered!=rendered {entry.dirty=true;}
                    entry.embed_previous_connected=connected;
                    entry.embed_previous_rendered=rendered;
                }
            }
            if let Some(realm)=owner.relevant_host_realm(engine.ctx()) {
                engine.ctx().with_host_realm(&realm,|ctx|owner.queue_object_tasks(ctx))
                    .map_err(browsing_context::host_realm_error)??;
            }
            Ok(())
        })();
        if let Err(error)=result {errors.push(error.to_value(engine.ctx()));}
    }
    errors
}
