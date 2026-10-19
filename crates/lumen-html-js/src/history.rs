//! Joint session history backed by bounded, attachment-free clone records.
use super::*;

const MAX_ENTRIES: usize = 256;
const MAX_STATE_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_BYTES: usize = 8 * 1024 * 1024;
const MAX_URL_BYTES: usize = 16 * 1024;

/// Entries share document state across a contiguous same-document segment.
/// A historical resource never retains a native document or its host realm.
pub(crate) struct DocumentState {
    pub document: RefCell<std::rc::Weak<DomRealm>>,
    pub resource: browsing_context::FrameSource,
    pub post_resource: Option<Rc<browsing_context::NavigationPostResource>>,
    pub metadata: browsing_context::NavigationMetadata,
    pub policy_container: RefCell<crate::csp::PolicyContainer>,
    pub base_url: String,
    pub origin: RefCell<browsing_context::Origin>,
    pub target_name: RefCell<String>,
    nested: RefCell<Vec<Rc<NavigableHistory>>>,
}

pub(crate) struct Entry {
    pub id: u64,
    pub step: Cell<Option<u64>>,
    pub url: RefCell<Box<str>>,
    pub state: RefCell<Box<[u8]>>,
    pub document: RefCell<Rc<DocumentState>>,
    pub scroll_manual: Cell<bool>,
    persisted_forms:RefCell<Option<Rc<[custom_elements::StoredCustomFormControl]>>>,
    restore_forms_pending:Cell<bool>,
}

fn persisted_form_bytes(entry:&Entry)->usize {
    entry.persisted_forms.borrow().as_ref().map_or(0,|values|values.iter().fold(0usize,|n,value|n.saturating_add(value.retained_bytes())))
}
pub(crate) fn restore_form_state_before_pageshow(ctx:&mut Ctx,document:&Rc<DomRealm>)->OpResult<()> {
    let Some(context)=document.browsing_context() else{return Ok(());};
    let history=navigable(&context)?;let entry=history.active.borrow().clone();
    if !entry.restore_forms_pending.replace(false){return Ok(());}
    let owner=entry.document_state().document.borrow().upgrade();
    if !owner.as_ref().is_some_and(|owner|Rc::ptr_eq(owner,document)){return Ok(());}
    let values=entry.persisted_forms.borrow().clone();
    if let Some(values)=values{custom_elements::restore_custom_form_state(ctx,document,&values)?;}
    Ok(())
}

impl Entry {
    pub(crate) fn url(&self) -> String { self.url.borrow().to_string() }
    pub(crate) fn document_state(&self) -> Rc<DocumentState> { self.document.borrow().clone() }
}

pub(crate) struct NavigableHistory {
    pub id: u64,
    pub entries: RefCell<Vec<Rc<Entry>>>,
    pub active: RefCell<Rc<Entry>>,
    pub current: RefCell<Rc<Entry>>,
}

enum HistoryOperation {
    Synchronous { navigable: u64, entry: Rc<Entry>, replace: Option<Rc<Entry>> },
    Traverse { delta: i32, source: u64 },
    Reload { source: u64 },
}

pub(crate) struct HistoryStore {
    root: RefCell<Option<Rc<NavigableHistory>>>,
    pub contexts: RefCell<HashMap<u64, std::rc::Weak<browsing_context::BrowsingContext>>>,
    current_step: Cell<u64>,
    next_entry: Cell<u64>,
    operations: RefCell<std::collections::VecDeque<HistoryOperation>>,
    scheduled: Cell<bool>,
    traversal: RefCell<Option<Traversal>>,
    traversing: Cell<bool>,
}

struct Traversal {
    step: u64,
    reload: bool,
    targets: Vec<(u64, Rc<Entry>)>,
    beforeunload: super::navigation_lifecycle::NavigationPhase,
    awaiting: HashSet<u64>,
    started: bool,
    processed: HashSet<u64>,
}

fn finalize_synchronous(store: &Rc<HistoryStore>, id: u64, entry: Rc<Entry>, replace: Option<Rc<Entry>>) -> OpResult<()> {
    let Some(context) = store.contexts.borrow().get(&id).and_then(std::rc::Weak::upgrade).filter(|context| context.is_active()) else { return Ok(()); };
    let history = navigable(&context)?;
    if !Rc::ptr_eq(&history.active.borrow(), &entry) { return Ok(()); }
    let step = if let Some(previous) = replace {
        let mut entries = history.entries.borrow_mut();
        let Some(index) = entries.iter().position(|candidate| Rc::ptr_eq(candidate, &previous)) else { return Ok(()); };
        let step = previous.step.get().unwrap_or(store.current_step.get());
        entries[index] = entry.clone(); step
    } else {
        store.clear_forward();
        let step = store.current_step.get().saturating_add(1);
        let mut entries = history.entries.borrow_mut();
        entries.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "history entry allocation"))?;
        entries.push(entry.clone()); step
    };
    entry.step.set(Some(step));
    *history.current.borrow_mut() = entry;
    store.current_step.set(step);
    Ok(())
}

fn jump_synchronous(store: &Rc<HistoryStore>, processed: &HashSet<u64>) -> OpResult<()> {
    loop {
        let index = store.operations.borrow().iter().position(|operation| matches!(operation,
            HistoryOperation::Synchronous { navigable, .. } if !processed.contains(navigable)));
        let Some(index) = index else { return Ok(()); };
        let operation = store.operations.borrow_mut().remove(index).expect("located synchronous history operation");
        if let HistoryOperation::Synchronous { navigable, entry, replace } = operation { finalize_synchronous(store, navigable, entry, replace)?; }
    }
}

pub(crate) fn before_deactivation(ctx:&mut Ctx, context: &Rc<browsing_context::BrowsingContext>) -> OpResult<()> {
    let Some(store) = context.history_store() else { return Ok(()); };
if let Some(document)=context.document() {
    let history=navigable(context)?;let entry=history.active.borrow().clone();
    let policy_container = document.policy_container();
    store.admit(entry.url.borrow().len().saturating_add(entry.state.borrow().len()).saturating_add(policy_container.retained_bytes()), Some(&entry))?;
    *entry.document_state().policy_container.borrow_mut() = policy_container;
    // Persisted user state is optional. Resource pressure drops this snapshot
    // without blocking navigation or retaining any old document.
    let snapshot=ctx.with_host_realm(&context.realm_handle(),|ctx|custom_elements::snapshot_custom_form_state(ctx,&document))
        .map_err(browsing_context::host_realm_error)?;
    let snapshot=snapshot.ok().filter(|values|!values.is_empty());
    let bytes=snapshot.as_ref().map_or(0,|values|values.iter().fold(0usize,|n,value|n.saturating_add(value.retained_bytes())));
    let cost=entry.url.borrow().len().saturating_add(entry.state.borrow().len()).saturating_add(bytes);
    *entry.persisted_forms.borrow_mut()=snapshot.filter(|_|store.admit(cost,Some(&entry)).is_ok()).map(|values|Rc::from(values.into_boxed_slice()));
}
    let traversal = store.traversal.borrow_mut().take();
    if let Some(mut traversal) = traversal {
        let result = jump_synchronous(&store, &traversal.processed);
        traversal.processed.insert(context.history_id());
        *store.traversal.borrow_mut() = Some(traversal);
        result?;
    }
    Ok(())
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self { root: RefCell::new(None), contexts: RefCell::new(HashMap::new()), current_step: Cell::new(0),
            next_entry: Cell::new(1), operations: RefCell::new(std::collections::VecDeque::new()), scheduled: Cell::new(false), traversal: RefCell::new(None), traversing: Cell::new(false) }
    }
}

struct QueueLease(Rc<HistoryStore>);
impl QueueLease { fn begin(self) -> Rc<HistoryStore> { self.0.clone() } }
impl Drop for QueueLease { fn drop(&mut self) { self.0.scheduled.set(false); } }

fn schedule(ctx: &mut Ctx, store: &Rc<HistoryStore>) -> OpResult<()> {
    if store.scheduled.replace(true) { return Ok(()); }
    let lease = QueueLease(store.clone());
    let root = store.contexts.borrow().get(&0).and_then(std::rc::Weak::upgrade)
        .ok_or_else(|| OpError::new("InvalidStateError", "history traversable is destroyed"))?;
    ctx.with_host_realm(&root.realm_handle(), |ctx| super::scheduling::queue_navigation_task(ctx, move |ctx| {
        let store = lease.begin();
        drain(ctx, &store)
    })).map_err(browsing_context::host_realm_error)??;
    Ok(())
}

fn enqueue(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>, operation: HistoryOperation) -> OpResult<()> {
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let synchronous = matches!(operation, HistoryOperation::Synchronous { .. });
    {
        let mut operations = store.operations.borrow_mut();
        if operations.len() >= MAX_ENTRIES { return Err(OpError::new("QuotaExceededError", "history traversal queue limit")); }
        operations.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "history traversal allocation"))?;
        operations.push_back(operation);
    }
    // An idle traversal queue can finalize synchronous navigation immediately.
    // A pending traversal keeps these tagged steps available to its interleave.
    if synchronous && !store.traversing.get()
        && store.operations.borrow().iter().all(|operation| matches!(operation, HistoryOperation::Synchronous { .. })) {
        return drain(ctx, &store);
    }
    schedule(ctx, &store)
}

pub(crate) fn reload(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>) -> OpResult<()> {
    navigable(context)?;
    enqueue(ctx, context, HistoryOperation::Reload { source: context.history_id() })
}

pub(crate) fn document_destroyed(document: &Rc<DomRealm>) {
    let Some(context) = document.browsing_context() else { return; };
    let Some(history) = context.history.borrow().clone() else { return; };
    let state = history.active.borrow().document_state();
    if state.document.borrow().upgrade().is_some_and(|active| Rc::ptr_eq(&active, document)) {
        *state.document.borrow_mut() = std::rc::Weak::new();
    }
}

pub(crate) fn child_removed(ctx: &mut Ctx, context: &browsing_context::BrowsingContext) -> OpResult<()> {
    if context.container_document().is_some_and(|document| document.lifecycle.destroyed.get()) { return Ok(()); }
    let Some(parent) = context.parent_context() else { return Ok(()); };
    let Some(document) = parent.document() else { return Ok(()); };
    if document.lifecycle.destroyed.get() { return Ok(()); }
    let Some(history) = parent.history.borrow().clone() else { return Ok(()); };
    history.active.borrow().document_state().nested.borrow_mut().retain(|history| history.id != context.history_id());
    if let Some(store) = parent.history_store() {
        if let Some(traversal) = store.traversal.borrow_mut().as_mut() { traversal.awaiting.remove(&context.history_id()); }
        store.contexts.borrow_mut().remove(&context.history_id());
        schedule(ctx, &store)?;
    }
    Ok(())
}

fn restore_state(ctx: &mut Ctx, document: &Rc<DomRealm>, entry: &Rc<Entry>) -> OpResult<Value> {
    let state = if entry.state.borrow().is_empty() { Value::Null } else {
        lumen_host::structured_clone::deserialize_for_storage(ctx, &entry.state.borrow()).unwrap_or(Value::Null)
    };
    *history_data(document).current_state.borrow_mut() = state.clone();
    Ok(state)
}

fn activate_same_document(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>, entry: &Rc<Entry>) -> OpResult<()> {
    let history = navigable(context)?;
    let previous = history.active.borrow().clone();
    *history.current.borrow_mut() = entry.clone();
    *history.active.borrow_mut() = entry.clone();
    if previous.id == entry.id { return Ok(()); }
    let Some(document) = context.document() else { return Ok(()); };
    let old_url = document.document_url().unwrap_or_else(|| previous.url());
    document.set_same_document_url(entry.url());
    ctx.with_host_realm(&context.realm_handle(), |ctx| {
        let state = restore_state(ctx, &document, entry)?;
        super::navigation_lifecycle::history_events(ctx, state, old_url, entry.url())
    }).map_err(browsing_context::host_realm_error)??;
    Ok(())
}

fn drain(ctx: &mut Ctx, store: &Rc<HistoryStore>) -> OpResult<()> {
    let result = drain_operations(ctx, store);
    if result.is_err() {
        // A failed population or bounded admission must not leave the shared
        // traversal queue permanently locked against later navigations.
        store.traversal.borrow_mut().take();
        store.traversing.set(false);
    }
    result
}

fn drain_operations(ctx: &mut Ctx, store: &Rc<HistoryStore>) -> OpResult<()> {
    let traversal = store.traversal.borrow_mut().take();
    if let Some(mut traversal) = traversal {
        if !traversal.beforeunload.finished() {
            *store.traversal.borrow_mut() = Some(traversal);
            return schedule(ctx, store);
        }
        if !traversal.beforeunload.accepted() { store.traversing.set(false); return schedule(ctx, store); }
        if !traversal.started {
            jump_synchronous(store, &traversal.processed)?;
            traversal.started = true;
            for (id, entry) in &traversal.targets {
                let context = store.contexts.borrow().get(id).and_then(std::rc::Weak::upgrade);
                let Some(context) = context.filter(|context| context.is_active()) else { continue; };
                let history = navigable(&context)?;
                *history.current.borrow_mut() = entry.clone();
                let same_document = !traversal.reload && context.document().is_some_and(|document| entry.document_state().document.borrow().upgrade().is_some_and(|target| Rc::ptr_eq(&document, &target)));
                if same_document {
                    traversal.processed.insert(*id);
                    activate_same_document(ctx, &context, entry)?;
                    if !entry.scroll_manual.get() {
                        if let Some(document)=context.document() { super::fragment::navigate(ctx,&document)?; }
                    }
                }
                else {
                    traversal.awaiting.insert(*id);
                    context.request_history_navigation(entry.clone());
                }
            }
        }
        if !traversal.awaiting.is_empty() { *store.traversal.borrow_mut() = Some(traversal); return Ok(()); }
        let step = store.used_steps().into_iter().filter(|step| *step <= traversal.step).max().unwrap_or(0);
        store.current_step.set(step);
        store.traversing.set(false);
    }
    loop {
        let operation = store.operations.borrow_mut().pop_front();
        let Some(operation) = operation else { return Ok(()); };
        match operation {
            HistoryOperation::Synchronous { navigable: id, entry, replace } => {
                finalize_synchronous(store, id, entry, replace)?;
            }
            HistoryOperation::Traverse { delta, source } => {
                let source = store.contexts.borrow().get(&source).and_then(std::rc::Weak::upgrade);
                if source.is_none_or(|context| !context.is_active()) { continue; }
                let steps = store.used_steps();
                let Some(index) = steps.iter().position(|step| *step == store.current_step.get()) else { continue; };
                let target_index = index as i64 + delta as i64;
                if target_index < 0 || target_index >= steps.len() as i64 { continue; }
                let target_step = steps[target_index as usize];
                let mut targets = Vec::new();
                let mut documents = Vec::new();
                let mut changed_ancestors = HashSet::new();
                let mut contexts: Vec<_> = store.contexts.borrow().values().filter_map(std::rc::Weak::upgrade).filter(|context| context.is_active()).collect();
                contexts.sort_by_key(|context| context.history_id());
                for context in contexts {
                    let mut ancestor = context.parent_context();
                    let mut blocked = false;
                    while let Some(parent) = ancestor { if changed_ancestors.contains(&parent.history_id()) { blocked = true; break; } ancestor = parent.parent_context(); }
                    if blocked { continue; }
                    let history = navigable(&context)?;
                    let target = history.entries.borrow().iter().filter(|entry| entry.step.get().is_some_and(|step| step <= target_step)).max_by_key(|entry| entry.step.get()).cloned();
                    let Some(target) = target else { continue; };
                    if !Rc::ptr_eq(&history.active.borrow(), &target) {
                        let current = context.document();
                        let cross_document = current.as_ref().is_none_or(|document| target.document_state().document.borrow().upgrade().is_none_or(|target| !Rc::ptr_eq(document, &target)));
                        if cross_document {
                            changed_ancestors.insert(context.history_id());
                            documents.extend(context.lifecycle_documents(ctx)?);
                        }
                        targets.push((context.history_id(), target));
                    }
                }
                let beforeunload = super::navigation_lifecycle::queue_phase(ctx, documents, false)?;
                store.traversing.set(true);
                *store.traversal.borrow_mut() = Some(Traversal { step: target_step, reload: false, targets, beforeunload, awaiting: HashSet::new(), started: false, processed: HashSet::new() });
                return schedule(ctx, store);
            }
            HistoryOperation::Reload { source } => {
                if let Some(context) = store.contexts.borrow().get(&source).and_then(std::rc::Weak::upgrade).filter(|context| context.is_active()) {
                    let entry = navigable(&context)?.active.borrow().clone();
                    let documents = context.lifecycle_documents(ctx)?;
                    let beforeunload = super::navigation_lifecycle::queue_phase(ctx, documents, false)?;
                    store.traversing.set(true);
                    *store.traversal.borrow_mut() = Some(Traversal { step: store.current_step.get(), reload: true, targets: vec![(source, entry)],
                        beforeunload, awaiting: HashSet::new(), started: false, processed: HashSet::new() });
                    return schedule(ctx, store);
                }
            }
        }
    }
}

pub(crate) fn navigation_finished(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>, target: Option<Rc<Entry>>, document: Option<&Rc<DomRealm>>, source: browsing_context::FrameSource, replace: bool) -> OpResult<()> {
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let history = navigable(context)?;
    if let Some(traversal) = store.traversal.borrow_mut().as_mut() { traversal.awaiting.remove(&history.id); }
    if let Some(target) = target {
        if let Some(document) = document {
            let origin = context.root_or_child_origin();
            let mut state = target.document_state();
            target.restore_forms_pending.set(state.document.borrow().upgrade().is_none() && target.persisted_forms.borrow().is_some());
            let final_url = document.document_url().unwrap_or_else(|| target.url());
            let redirected = lumen_common::url::parse(&final_url, None).ok().zip(lumen_common::url::parse(&target.url(), None).ok())
                .is_some_and(|(mut final_url, mut requested)| { final_url.fragment = None; requested.fragment = None; final_url.href() != requested.href() });
            if redirected {
                target.persisted_forms.borrow_mut().take();target.restore_forms_pending.set(false);
                *target.state.borrow_mut() = Box::new([]);
                let origin = state.origin.borrow().clone();
                let name = state.target_name.borrow().clone();
                let policy_container = state.policy_container.borrow().clone();
                state = Rc::new(DocumentState { document: RefCell::new(std::rc::Weak::new()), resource: state.resource.clone(),
                    post_resource: state.post_resource.clone(),
                    metadata: state.metadata.clone(),
                    policy_container: RefCell::new(policy_container),
                    base_url: state.base_url.clone(), origin: RefCell::new(origin),
                    target_name: RefCell::new(name), nested: RefCell::new(Vec::new()) });
                *target.document.borrow_mut() = state.clone();
                *target.url.borrow_mut() = final_url.into_boxed_str();
            }
            if !state.origin.borrow().same_origin(&origin) {
                target.persisted_forms.borrow_mut().take();target.restore_forms_pending.set(false);
                *target.state.borrow_mut() = Box::new([]);
                if context.parent_context().is_none() { state.target_name.borrow_mut().clear(); }
            }
            *state.origin.borrow_mut() = origin;
            context.restore_history_target_name(state.target_name.borrow().clone());
            *state.document.borrow_mut() = Rc::downgrade(document);
            document.set_same_document_url(target.url());
            *history.active.borrow_mut() = target.clone();
            *history.current.borrow_mut() = target.clone();
            ctx.with_host_realm(&context.realm_handle(), |ctx| restore_state(ctx, document, &target)).map_err(browsing_context::host_realm_error)??;
        }
    } else if let Some(document) = document {
        let previous = history.active.borrow().clone();
        let document_state = Rc::new(DocumentState { document: RefCell::new(Rc::downgrade(document)), resource: source,
            post_resource: context.take_post_resource(),
            metadata: context.captured_navigation_metadata(),
            policy_container: RefCell::new(document.policy_container()),
            base_url: document.base_url(), origin: RefCell::new(context.root_or_child_origin()), target_name: RefCell::new(context.target_name()), nested: RefCell::new(Vec::new()) });
        let url = document.document_url().unwrap_or_else(|| "about:blank".into());
        let source_bytes = document_state.resource.retained_inline_bytes();
        let post_bytes = document_state.post_resource.as_ref().map_or(0, |resource| post_resource_bytes(resource));
        store.admit(url.len().saturating_add(source_bytes).saturating_add(post_bytes).saturating_add(document_state.metadata.referrer.source.len())
            .saturating_add(document_state.metadata.policy_container.retained_bytes()).saturating_add(document_state.policy_container.borrow().retained_bytes()), replace.then_some(&previous))?;
        let entry = Rc::new(Entry { id: store.entry_id(), step: Cell::new(None), url: RefCell::new(url.into_boxed_str()), state: RefCell::new(Box::new([])), document: RefCell::new(document_state), scroll_manual: Cell::new(false), persisted_forms:RefCell::new(None),restore_forms_pending:Cell::new(false) });
        *history.active.borrow_mut() = entry.clone();
        enqueue(ctx, context, HistoryOperation::Synchronous { navigable: history.id, entry, replace: replace.then_some(previous) })?;
    }
    schedule(ctx, &store)
}

pub(crate) fn preflight_navigation(context: &Rc<browsing_context::BrowsingContext>, url: &str, resource: &browsing_context::FrameSource, replace: bool) -> OpResult<()> {
    if context.history_navigation_pending() { return Ok(()); }
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let history = navigable(context)?;
    let previous = history.active.borrow().clone();
    let replace = replace || context.document().is_some_and(|document| document.lifecycle.initial_about_blank.get());
    let source_bytes = resource.retained_inline_bytes();
    let post_bytes = context.pending_post_resource().as_ref().map_or(0, |resource| post_resource_bytes(resource));
    store.admit_queue()?;
    store.admit(url.len().saturating_add(source_bytes).saturating_add(post_bytes).saturating_add(context.captured_navigation_metadata().referrer.source.len()), replace.then_some(&previous))
}

fn post_resource_bytes(resource: &browsing_context::NavigationPostResource) -> usize {
    resource.headers.iter().fold(resource.body.len(), |bytes, (name, value)| bytes.saturating_add(name.len()).saturating_add(value.len()))
}

pub(crate) fn preflight_post_resource(context: &Rc<browsing_context::BrowsingContext>, resource: &browsing_context::NavigationPostResource) -> OpResult<()> {
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let history = navigable(context)?;
    let previous = history.active.borrow().clone();
    store.admit(post_resource_bytes(resource), context.replaces_history_entry().then_some(&previous))
}

impl HistoryStore {
    fn admit_queue(&self) -> OpResult<()> {
        let mut operations = self.operations.borrow_mut();
        if operations.len() >= MAX_ENTRIES { return Err(OpError::new("QuotaExceededError", "history traversal queue limit")); }
        operations.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "history traversal allocation"))
    }
    fn entry_id(&self) -> u64 { let id = self.next_entry.get(); self.next_entry.set(id.wrapping_add(1)); id }

    fn histories(&self) -> Vec<Rc<NavigableHistory>> {
        let mut histories = Vec::new();
        if let Some(root) = self.root.borrow().clone() { histories.push(root); }
        let mut seen = HashSet::new();
        let mut index = 0;
        while index < histories.len() {
            let entries = histories[index].entries.borrow().clone();
            for entry in entries {
                let document = entry.document_state();
                if !seen.insert(Rc::as_ptr(&document) as usize) { continue; }
                histories.extend(document.nested.borrow().iter().cloned());
            }
            index += 1;
        }
        histories
    }

    fn used_steps(&self) -> Vec<u64> {
        let mut steps: Vec<_> = self.histories().iter().flat_map(|history| history.entries.borrow().iter()
            .filter_map(|entry| entry.step.get()).collect::<Vec<_>>()).collect();
        steps.sort_unstable(); steps.dedup(); steps
    }

    fn clear_forward(&self) {
        for history in self.histories() { history.entries.borrow_mut().retain(|entry| entry.step.get().is_none_or(|step| step <= self.current_step.get())); }
    }

    fn admit(&self, cost: usize, replacing: Option<&Rc<Entry>>) -> OpResult<()> {
        let mut count = 0usize;
        let mut bytes = cost;
        let mut documents = HashSet::new();
        let mut entries_seen = HashSet::new();
        for history in self.histories() {
            for entry in history.entries.borrow().iter() {
                if replacing.is_some_and(|old| Rc::ptr_eq(old, entry)) { continue; }
                if replacing.is_none() && entry.step.get().is_some_and(|step| step > self.current_step.get()) { continue; }
                entries_seen.insert(entry.id);
                count += 1;
                bytes = bytes.saturating_add(entry.url.borrow().len()).saturating_add(entry.state.borrow().len()).saturating_add(persisted_form_bytes(entry));
                let document = entry.document_state();
                if documents.insert(Rc::as_ptr(&document) as usize) {
                    bytes = bytes.saturating_add(document.metadata.referrer.source.len())
                        .saturating_add(document.metadata.policy_container.retained_bytes())
                        .saturating_add(document.policy_container.borrow().retained_bytes());
                    bytes = bytes.saturating_add(document.resource.retained_inline_bytes());
                    if let Some(resource) = &document.post_resource { bytes = bytes.saturating_add(post_resource_bytes(resource)); }
                }
            }
        }
        for operation in self.operations.borrow().iter() {
            let HistoryOperation::Synchronous { entry, .. } = operation else { continue; };
            if !entries_seen.insert(entry.id) || replacing.is_some_and(|old| old.id == entry.id) { continue; }
            count += 1;
            bytes = bytes.saturating_add(entry.url.borrow().len()).saturating_add(entry.state.borrow().len()).saturating_add(persisted_form_bytes(entry));
            let document = entry.document_state();
            if documents.insert(Rc::as_ptr(&document) as usize) {
                bytes = bytes.saturating_add(document.metadata.referrer.source.len())
                    .saturating_add(document.metadata.policy_container.retained_bytes())
                    .saturating_add(document.policy_container.borrow().retained_bytes());
                bytes = bytes.saturating_add(document.resource.retained_inline_bytes());
                if let Some(resource) = &document.post_resource { bytes = bytes.saturating_add(post_resource_bytes(resource)); }
            }
        }
        if count >= MAX_ENTRIES || bytes > MAX_HISTORY_BYTES { return Err(OpError::new("QuotaExceededError", "traversable history storage limit")); }
        Ok(())
    }
}

pub(crate) fn navigable(context: &Rc<browsing_context::BrowsingContext>) -> OpResult<Rc<NavigableHistory>> {
    if let Some(history) = context.history.borrow().clone() { return Ok(history); }
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let document = context.document().ok_or_else(|| OpError::new("InvalidStateError", "history document unavailable"))?;
    let url = document.document_url().unwrap_or_else(|| "about:blank".into());
    let metadata = browsing_context::NavigationMetadata::from_document(&document);
    store.admit(url.len().saturating_add(metadata.referrer.source.len()).saturating_add(metadata.policy_container.retained_bytes()).saturating_add(document.policy_container().retained_bytes()), None)?;
    let state = Rc::new(DocumentState { document: RefCell::new(Rc::downgrade(&document)),
        post_resource: None,
        metadata,
        policy_container: RefCell::new(document.policy_container()),
        resource: if document.lifecycle.initial_about_blank.get() { browsing_context::FrameSource::Blank } else { browsing_context::FrameSource::Url(context.current_document_url()) },
        base_url: document.base_url(), origin: RefCell::new(context.root_or_child_origin()), target_name: RefCell::new(context.target_name()), nested: RefCell::new(Vec::new()) });
    let entry = Rc::new(Entry { id: store.entry_id(), step: Cell::new(Some(store.current_step.get())),
        url: RefCell::new(url.into_boxed_str()), state: RefCell::new(Box::new([])), document: RefCell::new(state),
        scroll_manual: Cell::new(false), persisted_forms:RefCell::new(None),restore_forms_pending:Cell::new(false) });
    let history = Rc::new(NavigableHistory { id: context.history_id(), entries: RefCell::new(vec![entry.clone()]),
        active: RefCell::new(entry.clone()), current: RefCell::new(entry) });
    if let Some(parent) = context.parent_context() {
        navigable(&parent)?.active.borrow().document_state().nested.borrow_mut().push(history.clone());
    } else { *store.root.borrow_mut() = Some(history.clone()); }
    *context.history.borrow_mut() = Some(history.clone());
    Ok(history)
}

fn history_data(realm: &Rc<DomRealm>) -> Rc<HistoryData> {
    if let Some(data) = realm.history_data.borrow().clone() { return data; }
    let data = Rc::new(HistoryData { current_state: RefCell::new(Value::Null) });
    *realm.history_data.borrow_mut() = Some(data.clone());
    data
}

pub(crate) fn fragment_navigation(ctx: &mut Ctx, context: &Rc<browsing_context::BrowsingContext>, url: &str, replace: bool) -> OpResult<bool> {
    if url.len() > MAX_URL_BYTES { return Err(error(ctx, "QuotaExceededError", "history URL limit")); }
    let Some(document) = context.document() else { return Ok(false); };
    let old_url = context.current_document_url();
    let Ok(mut old) = lumen_common::url::parse(&old_url, None) else { return Ok(false); };
    let Ok(mut next) = lumen_common::url::parse(url, None) else { return Ok(false); };
    let fragment = next.fragment.clone();
    old.fragment = None; next.fragment = None;
    if fragment.is_none() || old.href() != next.href() { return Ok(false); }
    let history = navigable(context)?;
    let store = context.history_store().ok_or_else(|| OpError::new("InvalidStateError", "history traversable unavailable"))?;
    let previous = history.active.borrow().clone();
    let replace = replace || document.lifecycle.initial_about_blank.get();
    store.admit(url.len(), replace.then_some(&previous)).map_err(|failure| storage_error(ctx, failure))?;
    store.admit_queue().map_err(|failure| storage_error(ctx, failure))?;
    let entry = Rc::new(Entry { id: store.entry_id(), step: Cell::new(None), url: RefCell::new(url.into()), state: RefCell::new(Box::new([])),
        document: RefCell::new(previous.document_state()), scroll_manual: Cell::new(previous.scroll_manual.get()), persisted_forms:RefCell::new(previous.persisted_forms.borrow().clone()),restore_forms_pending:Cell::new(false) });
    activate_same_document(ctx, context, &entry)?;
    super::fragment::navigate(ctx,&document)?;
    if !Rc::ptr_eq(&history.active.borrow(), &entry) { return Ok(true); }
    enqueue(ctx, context, HistoryOperation::Synchronous { navigable: history.id, entry, replace: replace.then_some(previous) })?;
    Ok(true)
}

pub(crate) struct HistoryData {
    current_state: RefCell<Value>,
}

impl HistoryData {
    pub(crate) fn trace_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.current_state.borrow());
    }
}

#[lumen_bind::class(name = "History", hint(js(webidl)))]
pub(crate) struct DomHistory {
    owner: std::rc::Weak<DomRealm>,
    data: Rc<HistoryData>,
}

impl lumen::embed::NativeIdentityOwner for DomHistory {
    const TRACES_NATIVE_VALUES: bool = true;

    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.data.trace_values(visit);
    }
}

impl DomHistory {
    fn active_owner(&self, ctx: &mut Ctx) -> OpResult<Rc<DomRealm>> {
        let realm = self.owner.upgrade().filter(|realm| realm.browsing_context()
            .is_some_and(|context| browsing_context::is_active_document(&context, realm)));
        realm.ok_or_else(|| error(ctx, "SecurityError", "history document is not fully active"))
    }

    fn update(&self, ctx: &mut Ctx, data: Value, url: Option<&str>, replace: bool) -> OpResult<()> {
        self.active_owner(ctx)?;
        // This runs authored property getters. Hold no native state/session
        // borrow and recheck the active document after serialization completes.
        let bytes = lumen_host::structured_clone::serialize_for_storage(ctx, &data, MAX_STATE_BYTES)
            .map_err(|failure| storage_error(ctx, failure))?;
        let realm = self.active_owner(ctx)?;
        let current_url = realm.document_url().unwrap_or_else(|| "about:blank".into());
        let next_url = match url {
            None | Some("") => current_url.clone(),
            Some(input) => {
                if input.len() > MAX_URL_BYTES { return Err(error(ctx, "QuotaExceededError", "history URL limit")); }
                let input = lumen::well_formed_utf8(input);
                let parsed = lumen_common::url::parse(&input, Some(&realm.base_url()))
                    .map_err(|_| error(ctx, "SecurityError", "invalid history URL"))?;
                let original = lumen_common::url::parse(&current_url, None)
                    .map_err(|_| error(ctx, "SecurityError", "invalid document URL"))?;
                if !can_rewrite(&original, &parsed) { return Err(error(ctx, "SecurityError", "history URL cannot rewrite this document URL")); }
                parsed.href()
            }
        };
        if next_url.len() > MAX_URL_BYTES { return Err(error(ctx, "QuotaExceededError", "history URL limit")); }
        let cost = next_url.len().checked_add(bytes.len())
            .ok_or_else(|| error(ctx, "QuotaExceededError", "history storage limit"))?;
        // The current JS state is materialized once and traced by both the
        // History wrapper and its Window. Older entries retain Rust bytes only.
        let context = realm.browsing_context()
            .ok_or_else(|| error(ctx, "SecurityError", "history document is not fully active"))?;
        let history = navigable(&context)?;
        let store = context.history_store().ok_or_else(|| error(ctx, "SecurityError", "history traversable unavailable"))?;
        let previous = history.active.borrow().clone();
        let replace = replace || realm.lifecycle.initial_about_blank.get();
        store.admit(cost, replace.then_some(&previous)).map_err(|failure| storage_error(ctx, failure))?;
        store.admit_queue().map_err(|failure| storage_error(ctx, failure))?;
        let owner = browsing_context::context_realm_handle(&context);
        let state = ctx.with_host_realm(&owner, |ctx| {
            lumen_host::structured_clone::deserialize_for_storage(ctx, &bytes)
                .map_err(|failure| storage_error(ctx, failure))
        }).map_err(browsing_context::host_realm_error)??;
        let entry = Rc::new(Entry { id: store.entry_id(), step: Cell::new(None), url: RefCell::new(next_url.clone().into_boxed_str()),
            state: RefCell::new(bytes.into_boxed_slice()), document: RefCell::new(previous.document_state()), scroll_manual: Cell::new(previous.scroll_manual.get()), persisted_forms:RefCell::new(previous.persisted_forms.borrow().clone()),restore_forms_pending:Cell::new(false) });
        *history.active.borrow_mut() = entry.clone();
        *self.data.current_state.borrow_mut() = state;
        // A same-document URL change retains the document's origin, including
        // inherited or opaque about/file origins, and uses shared base updates.
        realm.set_same_document_url(next_url);
        enqueue(ctx, &context, HistoryOperation::Synchronous { navigable: history.id, entry, replace: replace.then_some(previous) })?;
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomHistory {
    #[getter]
    fn length(&self, ctx: &mut Ctx) -> OpResult<u32> {
        let realm = self.active_owner(ctx)?;
        let context = realm.browsing_context().expect("fully active history context");
        navigable(&context)?;
        let store = context.history_store().expect("active history traversable");
        let pending = store.operations.borrow().iter().filter(|operation| matches!(operation, HistoryOperation::Synchronous { replace: None, .. })).count();
        Ok((store.used_steps().len() + pending) as u32)
    }

    #[getter]
    fn state(&self, ctx: &mut Ctx) -> OpResult<Value> {
        self.active_owner(ctx)?;
        Ok(self.data.current_state.borrow().clone())
    }

    #[method(name = "pushState", coerce)]
    fn push_state(&self, ctx: &mut Ctx, data: Value, _unused: &str, url: Option<&str>) -> OpResult<()> {
        self.update(ctx, data, url, false)
    }

    #[method(name = "replaceState", coerce)]
    fn replace_state(&self, ctx: &mut Ctx, data: Value, _unused: &str, url: Option<&str>) -> OpResult<()> {
        self.update(ctx, data, url, true)
    }

    #[method]
    fn go(&self, ctx: &mut Ctx, delta: Option<i32>) -> OpResult<()> {
        let realm = self.active_owner(ctx)?;
        let context = realm.browsing_context().expect("fully active history context");
        navigable(&context)?;
        let delta = delta.unwrap_or(0);
        enqueue(ctx, &context, if delta == 0 { HistoryOperation::Reload { source: context.history_id() } }
            else { HistoryOperation::Traverse { delta, source: context.history_id() } })
    }

    #[method]
    fn back(&self, ctx: &mut Ctx) -> OpResult<()> { self.go(ctx, Some(-1)) }

    #[method]
    fn forward(&self, ctx: &mut Ctx) -> OpResult<()> { self.go(ctx, Some(1)) }

    #[getter(name = "scrollRestoration")]
    fn scroll_restoration(&self, ctx: &mut Ctx) -> OpResult<&'static str> {
        let realm = self.active_owner(ctx)?;
        let history = navigable(&realm.browsing_context().expect("fully active history context"))?;
        let manual = history.active.borrow().scroll_manual.get();
        Ok(if manual { "manual" } else { "auto" })
    }

    #[setter(name = "scrollRestoration", coerce)]
    fn set_scroll_restoration(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        if !matches!(value, "auto" | "manual") { return Err(error(ctx, "TypeError", "invalid ScrollRestoration value")); }
        let realm = self.active_owner(ctx)?;
        navigable(&realm.browsing_context().expect("fully active history context"))?.active.borrow().scroll_manual.set(value == "manual");
        Ok(())
    }
}

fn error(ctx: &mut Ctx, name: &'static str, message: &'static str) -> OpError {
    crate::error_reporting::dom_exception(ctx, name, message)
}

fn storage_error(ctx: &mut Ctx, failure: OpError) -> OpError {
    match failure.class() {
        "DataCloneError" => crate::error_reporting::dom_exception(ctx, "DataCloneError", failure.message()),
        "QuotaExceededError" => crate::error_reporting::dom_exception(ctx, "QuotaExceededError", failure.message()),
        _ => failure,
    }
}

fn can_rewrite(original: &lumen_common::url::Url, next: &lumen_common::url::Url) -> bool {
    if original.scheme != next.scheme || original.username != next.username ||
        original.password != next.password || original.host != next.host || original.port != next.port {
        return false;
    }
    match original.scheme.as_str() {
        "http" | "https" => true,
        "file" => original.path == next.path,
        _ => original.path == next.path && original.query == next.query,
    }
}

pub(crate) fn value(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<Value> {
    if let Some(value) = realm.history_wrapper.borrow().as_ref().and_then(WeakValue::upgrade) { return Ok(value); }
    let context = realm.browsing_context().ok_or_else(|| error(ctx, "SecurityError", "history document is not fully active"))?;
    navigable(&context)?;
    let data = history_data(realm);
    let value = ctx.new_instance(DomHistory { owner: Rc::downgrade(realm), data });
    ctx.set_native_identity_owner::<DomHistory>(&value)?;
    *realm.history_wrapper.borrow_mut() = ctx.weak_value(&value);
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_history_restores_departing_policy_instead_of_current_initiator() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let parent = crate::install(runtime.engine().ctx(), "<iframe src='data:text/html,child'></iframe>", 128).unwrap();
        parent.set_document_url("https://creator.test/page");
        parent.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "img-src 'self'".into())]).unwrap();
        parent.set_navigation_referrer(None, &[("Referrer-Policy".into(), "no-referrer".into())]);
        let frame = parent.frame_contexts(runtime.engine().ctx()).unwrap().remove(0);
        let child = frame.install_response_for_request(runtime.engine().ctx(), &frame.navigation_request(), "data:text/html,child", "text/html", "<body></body>", 128).unwrap();
        assert_eq!(child.navigation_referrer().policy, lumen_common::referrer::ReferrerPolicy::NoReferrer);
        let context = child.browsing_context().unwrap();
        let history = navigable(&context).unwrap();
        let entry = history.active.borrow().clone();
        child.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "img-src 'none'".into())]).unwrap();
        child.set_navigation_referrer(None, &[("Referrer-Policy".into(), "origin".into())]);
        frame.prepare_document_deactivation(runtime.engine().ctx()).unwrap();
        let captured = entry.document_state().policy_container.borrow().clone();
        parent.set_content_security_policy_headers(&[("Content-Security-Policy".into(), "script-src 'none'".into())]).unwrap();
        parent.set_navigation_referrer(None, &[("Referrer-Policy".into(), "unsafe-url".into())]);
        context.request_history_navigation(entry);
        let restored = frame.install_response_for_request(runtime.engine().ctx(), &frame.navigation_request(), "data:text/html,child", "text/html", "<body></body>", 128).unwrap();
        assert_eq!(restored.policy_container(), captured);
        assert_eq!(restored.navigation_referrer().policy, lumen_common::referrer::ReferrerPolicy::Origin,
            "local history restores the departing document policy, rather than the current initiator");
        assert!(restored.module_fetch_policy_snapshot().unwrap().check("https://creator.test/image", "data:text/html,child", lumen_common::csp::Destination::Image).unwrap().blocked);
        assert!(!restored.module_fetch_policy_snapshot().unwrap().check_inline("globalThis.ran=true", None, lumen_common::csp::InlineCheckType::Script).unwrap().blocked);
        assert_eq!(frame.origin().serialize(), "null");
    }

    fn browser() -> (lumen_runtime::Runtime, Rc<DomRealm>) {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let realm = crate::install(runtime.engine().ctx(), "<!doctype html><html><head></head><body></body></html>", 1024).unwrap();
        realm.set_document_url("https://history.test/path/page.html");
        (runtime, realm)
    }

    fn check(runtime: &mut lumen_runtime::Runtime, source: &str) {
        let result = runtime.engine().eval_value(source).unwrap().unwrap_or_else(|exception| {
            let description = runtime.engine().ctx().member_get(&exception, "stack").ok()
                .and_then(|value| runtime.engine().ctx().coerce_string(&value).ok()).map(|value| value.to_string()).unwrap_or_default();
            panic!("history guard threw: {description}");
        });
        assert!(matches!(result, Value::Bool(true)), "history guard returned a non-true value");
    }

    #[test]
    fn specification_window_document_destruction_preserves_documentless_nested_history() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let parent = crate::install(runtime.engine().ctx(), "<!doctype html><iframe srcdoc='<p>child</p>'></iframe>", 1024).unwrap();
        parent.set_document_url("https://history.test/parent.html");
        let frame = parent.frame_contexts(runtime.engine().ctx()).unwrap().remove(0);
        let child = frame.install_response_for_request(runtime.engine().ctx(), &frame.navigation_request(),
            "about:srcdoc", "text/html", "<p>child</p>", 128).unwrap();
        let context = child.browsing_context().unwrap();
        let history = navigable(&context).unwrap();
        let parent_history = navigable(&parent.browsing_context().unwrap()).unwrap();
        let parent_state = parent_history.active.borrow().document_state();
        let weak_child = Rc::downgrade(&child);
        let weak_binding = Rc::downgrade(&context);
        let weak_global=runtime.engine().ctx().with_host_realm(&frame.realm_handle(),|ctx|{
            let global=ctx.global_object();ctx.weak_value(&global)
        }).expect("inspect child host realm").expect("child global identity");
        let global_identity=weak_global.upgrade().and_then(|global|global.object_identity());
        let weak_wrapper=child.document_wrapper.borrow().clone();
        let weak_hub=child.custom_element_hub.borrow().clone();
        let host_tasks=runtime.engine().ctx().host_mut::<lumen_host::TaskRegistry>()
            .map_or(0,|tasks|tasks.pending_for_realm(&frame.realm_handle()));
        let timer_tasks=runtime.engine().ctx().host_mut::<lumen_timers::Timers>()
            .map_or(0,|timers|timers.pending_for_realm(&frame.realm_handle()));
        super::super::navigation_lifecycle::destroy(runtime.engine().ctx(), &parent);
        let mut retired = Vec::new();
        parent.retire_all_frame_contexts(runtime.engine().ctx(), &mut retired);
        for realm in retired { runtime.engine().ctx().dispose_host_realm(&realm).expect("retire destroyed descendant realm"); }
        drop(child); drop(context); drop(frame);
        runtime.engine().collect_garbage();
        assert!(weak_binding.upgrade().is_none(), "history must not retain an obsolete native binding");
        assert!(weak_child.upgrade().is_none(), "a saved resource must not retain its old native document: document strong={}, global identity={global_identity:x?}, global alive={}, document wrapper alive={}, registry hub strong={}, admitted host tasks={}, timer tasks={}",
            weak_child.strong_count(),weak_global.upgrade().is_some(),
            weak_wrapper.as_ref().and_then(lumen::embed::WeakValue::upgrade).is_some(),weak_hub.strong_count(),host_tasks,timer_tasks);
        assert!(parent_state.nested.borrow().iter().any(|saved| Rc::ptr_eq(saved, &history)));
        let state = history.active.borrow().document_state();
        assert!(state.document.borrow().upgrade().is_none());
        assert!(matches!(&state.resource, browsing_context::FrameSource::SrcDoc(source) if source=="<p>child</p>"));
    }

    #[test]
    fn specification_window_history_event_dictionaries_use_webidl_conversion_order() {
        let (mut runtime, _realm) = browser();
        check(&mut runtime, r#"(() => {
            const reads=[];
            const init={};
            for(const name of ['bubbles','cancelable','composed','hasUAVisualTransition','state'])
                Object.defineProperty(init,name,{get(){reads.push(name);return name==='state'?{answer:42}:true;}});
            const pop=new PopStateEvent('popstate',init);
            if(reads.join(',')!=='bubbles,cancelable,composed,hasUAVisualTransition,state' ||
                !pop.bubbles || !pop.cancelable || !pop.composed || !pop.hasUAVisualTransition || pop.state.answer!==42)
                throw new Error('PopStateEvent dictionary conversion order');
            reads.length=0;
            const hashInit={};
            for(const name of ['bubbles','cancelable','composed'])
                Object.defineProperty(hashInit,name,{get(){reads.push(name);return false;}});
            for(const name of ['newURL','oldURL'])
                Object.defineProperty(hashInit,name,{get(){reads.push(name);return {toString(){reads.push(name+' conversion');return name;}};}});
            const hash=new HashChangeEvent('hashchange',hashInit);
            if(reads.join(',')!=='bubbles,cancelable,composed,newURL,newURL conversion,oldURL,oldURL conversion' ||
                hash.newURL!=='newURL' || hash.oldURL!=='oldURL')throw new Error('HashChangeEvent dictionary conversion order');
            reads.length=0;
            const marker={};let stopped=false;
            try{new HashChangeEvent('hashchange',{get newURL(){return {toString(){throw marker;}};},get oldURL(){reads.push('oldURL');}});}
            catch(error){stopped=error===marker;}
            return stopped && reads.length===0;
        })()"#);
    }

    #[test]
    fn specification_window_history_events_trace_state_and_preserve_native_interfaces() {
        let (mut runtime, realm) = browser();
        check(&mut runtime, r#"(() => {
            globalThis.observedHistoryEvents=[];
            globalThis.savedPop = new PopStateEvent('popstate', {state:{payload:new Map([['answer',42]])},hasUAVisualTransition:true});
            dispatchEvent(savedPop);
            addEventListener('popstate', event=>observedHistoryEvents.push([
                event instanceof PopStateEvent,event.isTrusted,event.target===window,event.state?.answer??null,event.hasUAVisualTransition]));
            globalThis.historyTargetTrace=[];
            addEventListener('popstate', event=>historyTargetTrace.push({window:event.target===window,
                globalThis:event.target===globalThis,currentWindow:event.currentTarget===window,
                targetConstructor:event.target?.constructor?.name,windowConstructor:window.constructor.name}));
            addEventListener('hashchange', event=>observedHistoryEvents.push([
                event instanceof HashChangeEvent,event.isTrusted,event.oldURL,event.newURL]));
            history.pushState({answer:42},'', '#first');
            history.pushState({answer:7},'', '#second');
            history.back();
            return true;
        })()"#);
        let store = realm.browsing_context().unwrap().history_store().unwrap();
        assert_eq!(store.used_steps(), vec![0,1,2]);
        assert_eq!(store.current_step.get(),2);
        assert!(super::super::scheduling::task_pending(runtime.engine().ctx()), "history.back must admit a navigation task");
        runtime.engine().collect_garbage();
        assert!(super::super::scheduling::task_pending(runtime.engine().ctx()), "GC must preserve the admitted traversal task");
        assert_eq!(store.operations.borrow().len(),1,"traversal operation must survive GC");
        assert!(store.contexts.borrow().get(&0).and_then(std::rc::Weak::upgrade)
            .is_some_and(|context| browsing_context::is_active_document(&context,&realm)),"traversal must retain the active root binding");
        let root = realm.browsing_context().unwrap();
        let history = navigable(&root).unwrap();
        assert!(history.entries.borrow().iter().all(|entry| entry.document_state().document.borrow().upgrade()
            .is_some_and(|document| Rc::ptr_eq(&document,&realm))),"same-document entries must preserve their actual live Document through GC");
        // This unit-test crate and lumen-runtime's normal dependency have
        // distinct op-state TypeIds. Drive this crate's real HTML task source;
        // the browser adapter guards exercise Runtime's single-instance loop.
        for _ in 0..8 {
            if !super::super::scheduling::task_pending(runtime.engine().ctx()) { break; }
            let errors = super::super::scheduling::run_navigation_tasks(runtime.engine(),64);
            assert!(errors.is_empty(),"navigation task failed");
            let errors = super::super::scheduling::run_tasks(runtime.engine(),64);
            assert!(errors.is_empty(),"HTML task failed");
        }
        assert!(!super::super::scheduling::task_pending(runtime.engine().ctx()),"history tasks must settle within the bounded guard turns");
        assert_eq!(runtime.fatal_exit_code(),None,"history owner-loop reported an uncaught native task error");
        assert_eq!(store.current_step.get(),1,"history traversal did not complete: queued={},scheduled={},traversing={},awaiting={:?}",
            store.operations.borrow().len(),store.scheduled.get(),store.traversing.get(),
            store.traversal.borrow().as_ref().map(|traversal|(&traversal.awaiting,traversal.beforeunload.finished(),traversal.beforeunload.accepted())));
        check(&mut runtime, r#"(() => { const valid=savedPop.state.payload.get('answer')===42 && savedPop.hasUAVisualTransition===true &&
            observedHistoryEvents.length===2 && observedHistoryEvents[0].join(',')==='true,true,true,42,false' &&
            observedHistoryEvents[1][0] && observedHistoryEvents[1][1] &&
            observedHistoryEvents[1][2].endsWith('#second') && observedHistoryEvents[1][3].endsWith('#first') &&
            history.state.answer===42;
            if(!valid)throw new Error(JSON.stringify({events:observedHistoryEvents,targets:historyTargetTrace,state:history.state,
                retained:savedPop.state.payload.get('answer'),transition:savedPop.hasUAVisualTransition,url:document.URL,length:history.length}));
            return true; })()"#);
    }

    #[test]
    fn history_updates_live_document_urls_and_preserves_parser_snapshots_and_cloned_state() {
        let (mut runtime, realm) = browser();
        let origin = realm.document_origin();
        check(&mut runtime, r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            const parser = new DOMParser();
            const before = parser.parseFromString('<p/>', 'text/html');
            const source = {map: new Map([['answer', 42]]), bytes: new Uint8Array([1, 2, 3])};
            source.self = source;
            let events = 0;
            addEventListener('popstate', () => ++events);
            addEventListener('hashchange', () => ++events);
            const canonicalClone = structuredClone;
            globalThis.structuredClone = () => { throw new Error('authored clone bridge'); };
            history.pushState(source, '', '../next.html#mark');
            globalThis.structuredClone = canonicalClone;
            check(history instanceof History && history === window.history && history.length === 2, 'history identity and length');
            check(document.URL === 'https://history.test/next.html#mark' && location.href === document.URL &&
                document.baseURI === document.URL, 'live URL and fallback base');
            check(before.URL === 'https://history.test/path/page.html' && before.baseURI === before.URL, 'parser snapshot');
            const after = parser.parseFromString('<p/>', 'text/html');
            check(after.URL === document.URL && after.baseURI === document.URL, 'parser uses current URL');
            const state = history.state;
            check(state !== source && state.self === state && state.map.get('answer') === 42 &&
                state.bytes[2] === 3 && state === history.state, 'structured state and cached identity');
            source.bytes[2] = 99;
            check(state.bytes[2] === 3, 'state owns copied bytes');
            const base = document.createElement('base'); base.href = '/frozen/'; document.head.append(base);
            history.replaceState({replaced: true}, '', 'child.html');
            check(history.length === 2 && history.state.replaced &&
                document.URL === 'https://history.test/frozen/child.html' &&
                document.baseURI === 'https://history.test/frozen/' && events === 0, 'replace, frozen base and no navigation events');
            history.state.owner = history;
            return true;
        })()"#);
        assert_eq!(origin, realm.document_origin());
        runtime.engine().collect_garbage();
        check(&mut runtime, "history.state.replaced === true && history.state.owner === history && history === window.history");
    }

    #[test]
    fn history_failures_are_branded_atomic_and_serialization_can_reenter() {
        let (mut runtime, _realm) = browser();
        check(&mut runtime, r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            history.replaceState({initial: true}, '', null);
            const initial = history.state, url = document.URL;
            for (const bad of ['https://other.test/path', 'http://history.test/path', 'https://user@history.test/path']) {
                let caught = false;
                try { history.pushState({}, '', bad); } catch (error) {
                    caught = error instanceof DOMException && error.name === 'SecurityError' && error.code === 18;
                }
                check(caught && history.length === 1 && history.state === initial && document.URL === url, 'atomic URL rejection');
            }
            const DOMError = DOMException;
            globalThis.DOMException = function () { throw new Error('authored exception constructor'); };
            let cloneError = false;
            try { history.pushState(() => {}, '', '/bad'); } catch (error) {
                cloneError = error instanceof DOMError && error.name === 'DataCloneError' && error.code === 25;
            }
            globalThis.DOMException = DOMError;
            check(cloneError && history.length === 1 && history.state === initial && document.URL === url, 'captured clone exception and atomicity');
            const bridge = globalThis.__lumenPortClone, resolve = globalThis.__lumenCloneResolve;
            globalThis.__lumenPortClone = {get isPort() {throw new Error('authored port bridge');}};
            globalThis.__lumenCloneResolve = () => {throw new Error('authored host resolver');};
            const ordinary = {ordinary: true};
            Object.defineProperty(ordinary, Symbol.for('lumen.transferable.clone'), {
                get() {throw new Error('nonstandard clone symbol getter');}
            });
            history.replaceState(ordinary, '');
            globalThis.__lumenPortClone = bridge; globalThis.__lumenCloneResolve = resolve;
            check(history.state.ordinary && history.length === 1, 'storage uses no authored transfer bridge');
            const marker = {};
            try { history.pushState({get fail() {throw marker;}}, '', '/bad'); } catch (error) { check(error === marker, 'getter error identity'); }
            let conversions = 0;
            history.pushState({get value() { history.pushState({inner: true}, '', '/inner'); return 7; }}, '',
                {toString() {++conversions; return '/outer';}});
            check(history.length === 3 && history.state.value === 7 && document.URL.endsWith('/outer') && conversions === 1,
                'reentrant serialization with once-only URL conversion');
            let receiver = false;
            try { History.prototype.pushState.call({}, null, ''); } catch (error) { receiver = error instanceof TypeError; }
            check(receiver, 'native receiver');
            return true;
        })()"#);
    }

    #[test]
    fn history_retained_records_enforce_limits_without_partial_publication() {
        let (mut runtime, realm) = browser();
        check(&mut runtime, r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            history.replaceState({initial: true}, '');
            const initial = history.state;
            let oversized = false;
            try { history.pushState('x'.repeat(1024 * 1024 + 1), '', '/oversized'); }
            catch (error) { oversized = error instanceof DOMException && error.name === 'DataCloneError'; }
            check(oversized && history.length === 1 && history.state === initial && document.URL.endsWith('/page.html'), 'clone byte budget');
            for (let i = 1; i < 256; ++i) history.pushState(i, '', '#entry-' + i);
            const last = history.state, url = document.URL;
            let full = false;
            try { history.pushState(256, '', '#overflow'); }
            catch (error) {full = error instanceof DOMException && error.name === 'QuotaExceededError';}
            check(full && history.length === 256 && history.state === last && document.URL === url, 'count budget atomicity');
            history.replaceState(null, '', '#replacement');
            check(history.length === 256 && history.state === null && document.URL.endsWith('#replacement'), 'replace at capacity');
            return true;
        })()"#);
        let context = realm.browsing_context().unwrap();
        let store = context.history_store().unwrap();
        let histories = store.histories();
        assert_eq!(histories.iter().map(|history| history.entries.borrow().len()).sum::<usize>(), MAX_ENTRIES);
        let history = navigable(&context).unwrap();
        let active = history.active.borrow().clone();
        assert!(store.admit(0, Some(&active)).is_ok());
    }

    #[test]
    fn history_checks_active_document_and_preserves_opaque_url_identity() {
        let (mut runtime, realm) = browser();
        realm.set_document_url("about:blank");
        let origin = realm.document_origin();
        check(&mut runtime, r#"(() => {
            history.pushState({kept: true}, '', '#allowed');
            const previous = history.state;
            let rejected = false;
            try {history.replaceState(null, '', 'about:srcdoc');}
            catch (error) {rejected = error instanceof DOMException && error.name === 'SecurityError';}
            if (!rejected || history.state !== previous || document.URL !== 'about:blank#allowed') throw new Error('opaque URL rewrite rules');
            globalThis.retainedOldHistory = history;
            return true;
        })()"#);
        assert_eq!(origin, realm.document_origin());
        let fresh = crate::install(runtime.engine().ctx(), "<!doctype html><p>new active document</p>", 128).unwrap();
        fresh.set_document_url("https://history.test/new-document.html");
        check(&mut runtime, r#"(() => {
            let failures = 0;
            for (const access of [() => retainedOldHistory.state, () => retainedOldHistory.length,
                () => retainedOldHistory.pushState(null, '', '/inactive'),
                () => retainedOldHistory.replaceState(null, '', '/inactive')]) {
                try {access();} catch (error) {if (error instanceof DOMException && error.name === 'SecurityError') ++failures;}
            }
            if (failures !== 4 || history === retainedOldHistory || history.length !== 1 ||
                document.URL !== 'https://history.test/new-document.html') throw new Error('active document history isolation');
            return true;
        })()"#);
        assert_eq!(realm.document_url().as_deref(), Some("about:blank#allowed"));
    }

    #[test]
    fn history_materializes_state_in_its_owning_realm_when_called_from_another() {
        let (mut runtime, _realm) = browser();
        let engine = runtime.engine();
        let handle = engine.ctx().create_host_realm();
        let (_foreign, history, array) = match engine.ctx().with_host_realm(&handle, |ctx| {
            let realm = crate::install(ctx, "<!doctype html><p>foreign</p>", 128).unwrap();
            realm.set_document_url("https://history.test/foreign.html");
            let global = ctx.global_object();
            let history = ctx.member_get(&global, "history").ok().expect("foreign history");
            let array = ctx.member_get(&global, "Array").ok().expect("foreign array constructor");
            (realm, history, array)
        }) { Ok(values) => values, Err(_) => panic!("enter foreign history realm") };
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, "foreignHistory", history).ok().expect("expose foreign history");
        engine.ctx().member_set(&global, "ForeignArray", array).ok().expect("expose foreign array constructor");
        check(&mut runtime, r#"(() => {
            foreignHistory.pushState([1, 2], '', '#foreign');
            if (!(foreignHistory.state instanceof ForeignArray) || foreignHistory.state instanceof Array ||
                document.URL !== 'https://history.test/path/page.html' || history.length !== 1)
                throw new Error('owning realm state and document isolation');
            return true;
        })()"#);
        runtime.engine().collect_garbage();
        check(&mut runtime, "foreignHistory.state instanceof ForeignArray && foreignHistory.state[1] === 2");
    }
}
