use super::*;
use lumen_html::graph::DocumentGraph;

pub(crate) struct ArenaGraph { pub(crate) owners: Vec<Rc<DomRealm>>, ids: Vec<u64>, limit: usize }

impl ArenaGraph {
    pub(crate) fn new(root: &Rc<DomRealm>) -> OpResult<Self> {
        let mut graph = Self { owners: vec![root.clone()], ids: Vec::new(), limit: 0 };
        let mut cursor = 0;
        while cursor < graph.owners.len() {
            let realm = graph.owners[cursor].clone();
            let dependencies = realm.template_graph_owners.borrow().values().filter_map(std::rc::Weak::upgrade).collect::<Vec<_>>();
            for owner in dependencies {
                if !graph.owners.iter().any(|old| Rc::ptr_eq(old, &owner)) {
                    graph.owners.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "template arena graph allocation"))?;
                    graph.owners.push(owner);
                }
            }
            graph.limit = graph.limit.checked_add(realm.session.borrow().document().node_count())
                .ok_or_else(|| OpError::new("QuotaExceededError", "template arena graph bound"))?;
            graph.ids.push(realm.session.borrow().document().root().document_id());
            cursor += 1;
        }
        Ok(graph)
    }

    pub(crate) fn owner(&self, node: NodeId) -> OpResult<Rc<DomRealm>> {
        self.ids.iter().position(|id| *id == node.document_id()).map(|index| self.owners[index].clone())
            .ok_or_else(|| OpError::new("InvalidStateError", "template arena owner was reclaimed"))
    }

    pub(crate) fn synchronize_owners(&self) -> OpResult<()> {
        for realm in &self.owners {
            let remotes = realm.session.borrow().document().foreign_template_links().iter().map(|(_, remote)| *remote).collect::<Vec<_>>();
            let mut dependencies = HashMap::new();
            for remote in remotes { let owner = self.owner(remote)?; dependencies.insert(remote.document_id(), Rc::downgrade(&owner)); }
            *realm.template_graph_owners.borrow_mut() = dependencies;
        }
        Ok(())
    }
}

impl DocumentGraph for ArenaGraph {
    type Read<'a> = std::cell::Ref<'a, lumen_html::Document>;
    fn read(&self, node: NodeId) -> Result<Self::Read<'_>, Error> {
        let owner = &self.owners[self.ids.iter().position(|id| *id == node.document_id()).ok_or(Error::InvalidNode)?];
        let document = std::cell::Ref::map(owner.session.borrow(), |session| session.document());
        document.kind(node)?;
        Ok(document)
    }
    fn node_limit(&self) -> usize { self.limit }
}

pub(crate) fn migrate_edges(ctx: &mut Ctx, source: &Rc<DomRealm>, target: &Rc<DomRealm>, mapping: &[(NodeId, NodeId)]) -> OpResult<()> {
    let source_root = source.session.borrow().document().root();
    let target_root = target.session.borrow().document().root();
    for (&id, owner) in source.template_graph_owners.borrow().iter() { target.template_graph_owners.borrow_mut().insert(id, owner.clone()); }
    source.template_graph_owners.borrow_mut().insert(target_root.document_id(), Rc::downgrade(target));
    target.template_graph_owners.borrow_mut().insert(source_root.document_id(), Rc::downgrade(source));
    let graph = ArenaGraph::new(source)?;
    for owner in &graph.owners { owner.session.borrow_mut().document_mut().remap_foreign_template_links(mapping).map_err(dom_error)?; }
    // Both associated endpoints use ordinary cached wrappers and ordinary GC
    // trace edges. Only these rare split-arena associations need identities
    // established eagerly; the dependency index itself remains weak.
    let mut pins = Vec::new();
    for owner in &graph.owners {
        let links = owner.session.borrow().document().foreign_template_links().to_vec();
        for (local, remote) in links {
            pins.push(owner.wrap(ctx, local));
            let remote_owner = graph.owner(remote)?;
            pins.push(remote_owner.wrap(ctx, remote));
        }
    }
    graph.synchronize_owners()?;
    for owner in &graph.owners { install_ancestor_resolver(owner); }
    drop(pins);
    Ok(())
}

pub(crate) fn copy_clone_state(ctx: &mut Ctx, graph: &ArenaGraph, target: &Rc<DomRealm>, pairs: &[(NodeId, NodeId)], fallback: Option<Rc<custom_elements::CustomElementHub>>) -> OpResult<()> {
    for source in &graph.owners {
        let source_id = source.session.borrow().document().root().document_id();
        let local = pairs.iter().filter(|(old, _)| old.document_id() == source_id).copied().collect::<Vec<_>>();
        if local.is_empty() { continue; }
        let snapshot = forms::clone_state_snapshot(&source.forms.borrow(), &local);
        forms::clone_live_values_into(&snapshot, &mut target.forms.borrow_mut(), target.session.borrow().document(), &local);
        if Rc::ptr_eq(source, target) { target.scripts.borrow_mut().clone_states(&local); }
        else { target.scripts.borrow_mut().clone_states_from(&source.scripts.borrow(), &local); }
        custom_elements::clone_registry_associations(ctx, source, target, &local, fallback.clone())?;
    }
    Ok(())
}

pub(crate) fn clone_into(ctx: &mut Ctx, source: &Rc<DomRealm>, root: NodeId, target: &Rc<DomRealm>, owner: NodeId, deep: bool, fallback: Option<Rc<custom_elements::CustomElementHub>>) -> OpResult<Value> {
    let graph = ArenaGraph::new(source)?;
    let plan = lumen_html::graph::clone_plan(&graph, root, deep).map_err(dom_error)?;
    let required = target.session.borrow().document().graph_clone_allocation_count(&plan, false).map_err(dom_error)?;
    target.prepare_allocation(ctx, required)?;
    let (copy, pairs) = target.session.borrow_mut().document_mut().clone_graph_plan(&graph, &plan, owner, false).map_err(dom_error)?;
    let _retention = NodeRetention::new(target, copy);
    target.defer_detached_root(copy);
    copy_clone_state(ctx, &graph, target, &pairs, fallback)?;
    event_content_handlers::initialize_subtree(ctx, target, copy)?;
    custom_elements::upgrade_cloned_or_parsed_subtree(ctx, target, copy)?;
    Ok(target.wrap(ctx, copy))
}

pub(crate) fn readopt_contents(ctx: &mut Ctx, realm: &Rc<DomRealm>, mapping: &[(NodeId, NodeId)]) -> OpResult<()> {
    for &(_, node) in mapping {
        let content = realm.session.borrow().document().template_content(node).map_err(dom_error)?;
        let Some(content) = content else { continue; };
        let owner = {
            let mut session = realm.session.borrow_mut();
            let document = session.document_mut();
            let host_owner = document.node_document(node).map_err(dom_error)?;
            if document.is_template_owner_document(host_owner) { host_owner }
            else { document.ensure_template_owner_document().map_err(dom_error)? }
        };
        let graph = ArenaGraph::new(realm)?;
        let source = graph.owner(content)?;
        if Rc::ptr_eq(realm, &source) && source.session.borrow().document().node_document(content).map_err(dom_error)? == owner { continue; }
        DomRealm::adopt_node_from_to_owner(realm, ctx, &source, content, owner, |_, _| Ok(()))?;
    }
    Ok(())
}

/// The local arena can already be borrowed by a core mutation validator. Read
/// it directly while borrowing only the genuinely foreign arenas.
enum GraphRead<'a> {
    Local(&'a lumen_html::Document),
    Foreign(std::cell::Ref<'a, lumen_html::Document>),
}
impl std::ops::Deref for GraphRead<'_> {
    type Target = lumen_html::Document;
    fn deref(&self) -> &Self::Target { match self { Self::Local(document) => document, Self::Foreign(document) => document } }
}
struct BorrowedArenaGraph<'a> { local: &'a lumen_html::Document, owners: Vec<Rc<DomRealm>>, ids: Vec<u64>, limit: usize }
impl DocumentGraph for BorrowedArenaGraph<'_> {
    type Read<'a> = GraphRead<'a> where Self: 'a;
    fn read(&self, node: NodeId) -> Result<Self::Read<'_>, Error> {
        if node.document_id() == self.local.root().document_id() { self.local.kind(node)?; return Ok(GraphRead::Local(self.local)); }
        let index = self.ids.iter().position(|id| *id == node.document_id()).ok_or(Error::InvalidNode)?;
        let document = std::cell::Ref::map(self.owners[index].session.borrow(), |session| session.document());
        document.kind(node)?;
        Ok(GraphRead::Foreign(document))
    }
    fn node_limit(&self) -> usize { self.limit }
}

pub(crate) fn install_ancestor_resolver(realm: &Rc<DomRealm>) {
    let weak = Rc::downgrade(realm);
    realm.session.borrow_mut().document_mut().set_template_ancestor_resolver(Rc::new(move |local, ancestor, node| {
        let realm = weak.upgrade().ok_or(Error::InvalidNode)?;
        let mut graph = BorrowedArenaGraph { local, owners: Vec::new(), ids: Vec::new(), limit: local.node_count() };
        let mut pending = realm.template_graph_owners.borrow().values().filter_map(std::rc::Weak::upgrade).collect::<Vec<_>>();
        while let Some(owner) = pending.pop() {
            if Rc::ptr_eq(&owner, &realm) || graph.owners.iter().any(|old| Rc::ptr_eq(old, &owner)) { continue; }
            pending.extend(owner.template_graph_owners.borrow().values().filter_map(std::rc::Weak::upgrade));
            let session = owner.session.borrow();
            graph.limit = graph.limit.checked_add(session.document().node_count()).ok_or(Error::LimitExceeded)?;
            graph.ids.push(session.document().root().document_id());
            drop(session);
            graph.owners.push(owner);
        }
        lumen_html::graph::host_including_ancestor(&graph, ancestor, node)
    }));
}

pub(crate) fn preflight_adoption(ctx: &mut Ctx, source: &Rc<DomRealm>, root: NodeId, target: &Rc<DomRealm>) -> OpResult<()> {
    preflight_adoption_roots(ctx, source, &[root], target)
}

pub(crate) fn preflight_adoption_roots(ctx: &mut Ctx, source: &Rc<DomRealm>, roots: &[NodeId], target: &Rc<DomRealm>) -> OpResult<()> {
    let graph = ArenaGraph::new(source)?;
    let target_id = target.session.borrow().document().root().document_id();
    let mut count = 0usize;
    let mut has_template = false;
    for &root in roots {
      let nodes = lumen_html::graph::identity_nodes(&graph, root).map_err(dom_error)?;
      for node in nodes {
        if node.document_id() != target_id { count = count.checked_add(1).ok_or_else(|| dom_error(Error::LimitExceeded))?; }
        if graph.read(node).map_err(dom_error)?.template_content(node).map_err(dom_error)?.is_some() { has_template = true; }
    }
    }
    if has_template && target.session.borrow().document().template_owner_document().is_none() { count = count.checked_add(1).ok_or_else(|| dom_error(Error::LimitExceeded))?; }
    target.prepare_allocation(ctx, count)
}
