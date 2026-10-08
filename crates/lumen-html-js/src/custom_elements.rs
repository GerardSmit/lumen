//! Custom element definitions, upgrades, and lifecycle reaction collection.
//!
//! The JS adapter owns constructor values and callbacks; node identity and tree mutations remain
//! in `lumen-html`. The hub is installed per JavaScript realm and attached to the document's
//! mutation fanout, so custom-element reactions do not replace mutation/observer consumers.
use super::*;
use lumen::embed::{Deferred, JsFunction, JsHost};
use lumen_bind::{CtorRet, Host, IntoError};
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_REGISTRY_ENTRIES: usize = 65_536;

#[derive(Clone)]
struct Definition {
    name: String,
    extends: Option<String>,
    constructor: Value,
    observed_attributes: HashSet<String>,
    callbacks: HashMap<&'static str, Value>,
    disabled_shadow: bool,
    disabled_internals: bool,
    form_associated:bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RealmNode {
    realm: usize,
    node: NodeId,
}

impl RealmNode {
    fn new(realm: &Rc<DomRealm>, node: NodeId) -> Self {
        Self {
            realm: Rc::as_ptr(realm) as usize,
            node,
        }
    }
}

// Publication occurs under parser arena borrows; document wrappers are created
// at invocation. Native retention keeps the exact logical document alive.
#[derive(Clone)]
struct AdoptedDocument {
    realm:Rc<DomRealm>,
    node:NodeId,
    _retention:Rc<NodeRetention>,
}
impl AdoptedDocument {
    fn new(realm:&Rc<DomRealm>,node:NodeId)->Self {
        Self{realm:realm.clone(),node,_retention:Rc::new(NodeRetention::new(realm,node))}
    }
    fn value(&self,ctx:&mut Ctx)->Value {self.realm.wrap(ctx,self.node)}
}

#[derive(Clone)]
enum Reaction {

    Upgrade(RealmNode),
    Connected(RealmNode, Value),
    Disconnected(RealmNode, Value),
    Attribute(
        RealmNode,
        Value,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
    ),
    Adopted(RealmNode, Value, Value, Value),
    AdoptedPending(RealmNode, Value, AdoptedDocument, AdoptedDocument),
    FormAssociated(RealmNode, Value, Option<NodeId>, Option<Rc<NodeRetention>>),
    FormDisabled(RealmNode, Value, bool),
    FormReset(RealmNode, Value),
    FormRestore(RealmNode,Value,Value),
    MoveFallback(RealmNode, Option<Value>, Option<Value>),
}

impl Reaction {
    fn node(&self) -> RealmNode {
        match self {
            Self::Upgrade(node) | Self::Connected(node, _) | Self::Disconnected(node, _) | Self::Attribute(node, ..)
            | Self::Adopted(node, ..) | Self::AdoptedPending(node, ..) | Self::MoveFallback(node, ..) | Self::FormAssociated(node, ..) | Self::FormDisabled(node, ..) | Self::FormReset(node, ..) | Self::FormRestore(node, ..) => *node,
        }
    }

    fn set_node(&mut self, key: RealmNode) {
        match self {
            Self::Upgrade(node) | Self::Connected(node, _) | Self::Disconnected(node, _) | Self::Attribute(node, ..)
            | Self::Adopted(node, ..) | Self::AdoptedPending(node, ..) | Self::MoveFallback(node, ..) | Self::FormAssociated(node, ..) | Self::FormDisabled(node, ..) | Self::FormReset(node, ..) | Self::FormRestore(node, ..) => *node = key,
        }
    }
}

/// HTML's element queue and per-element reaction queues. Only elements with
/// pending reactions allocate state; reentrant callbacks append to the current
/// element's queue, which is drained before advancing to another element.
const MAX_REACTIONS: usize = 100_000;
const MAX_REACTION_SCOPES: usize = 1024;

struct ElementQueueEntry {
    hub: Rc<CustomElementHub>,
    key: RealmNode,
    realm: Rc<DomRealm>,
    _wrapper: Option<Value>,
    _retention: NodeRetention,
}

type ElementQueue = VecDeque<ElementQueueEntry>;

/// The HTML reactions stack is shared by the agent, including its host realms.
/// Entries retain their registry only until delivery; empty frames allocate no
/// element storage and there is no state attached to ordinary DOM nodes.
#[derive(Default)]
struct AgentReactions {
    stack: Vec<ElementQueue>,
    backup: ElementQueue,
    processing_backup: bool,
    backup_scheduled: bool,
    entries: usize,
    reactions: usize,
    active_constructors: Vec<(Value, std::rc::Weak<CustomElementHub>)>,
    invoking: usize,
    invoked: usize,
    invocation_failed: bool,
}

impl AgentReactions {
    fn push_scope(&mut self) -> OpResult<()> {
        if self.stack.len() >= MAX_REACTION_SCOPES {
            return Err(OpError::new("QuotaExceededError", "custom element reaction scope limit"));
        }
        self.stack.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element reaction scope allocation"))?;
        self.stack.push(VecDeque::new());
        Ok(())
    }

    fn enqueue(&mut self, hub: Rc<CustomElementHub>, key: RealmNode) -> bool {
        if self.entries >= MAX_REACTIONS || self.reactions >= MAX_REACTIONS { return false; }
        let queue = self.stack.last_mut().unwrap_or(&mut self.backup);
        if queue.try_reserve(1).is_err() { return false; }
        let realm = hub.realm_for_node(key).unwrap_or_else(|| hub.realm.clone());
        {
            let mut retained = realm.retained_nodes.borrow_mut();
            if !retained.contains_key(&key.node) && retained.try_reserve(1).is_err() { return false; }
        }
        let wrapper = realm.wrappers.borrow().get(&key.node).and_then(WeakValue::upgrade);
        let retention = NodeRetention::new(&realm, key.node);
        queue.push_back(ElementQueueEntry {hub, key, realm, _wrapper: wrapper, _retention: retention});
        self.entries += 1;
        self.reactions += 1;
        true
    }

    fn schedule_backup(&mut self) -> bool {
        if self.backup.is_empty() || self.processing_backup || self.backup_scheduled { return false; }
        self.backup_scheduled = true;
        true
    }

}

fn agent_reactions(ctx: &mut Ctx) -> Rc<RefCell<AgentReactions>> {
    if let Some(agent) = ctx.op_state().get::<Rc<RefCell<AgentReactions>>>() { return agent.clone(); }
    let agent = Rc::new(RefCell::new(AgentReactions::default()));
    ctx.op_state().put(agent.clone());
    agent
}

pub(crate) fn begin_reaction_scope(ctx: &mut Ctx) -> Result<(), Value> {
    let result = agent_reactions(ctx).borrow_mut().push_scope();
    result.map_err(|error| error.to_value(ctx))
}

fn invoke_element_queue(ctx: &mut Ctx, agent: &Rc<RefCell<AgentReactions>>, mut queue: ElementQueue) {
    {
        let mut state = agent.borrow_mut();
        if state.invoking == 0 { state.invoked = 0; state.invocation_failed = false; }
        state.invoking += 1;
    }
    struct Invocation(Rc<RefCell<AgentReactions>>);
    impl Drop for Invocation {
        fn drop(&mut self) { self.0.borrow_mut().invoking -= 1; }
    }
    let _invocation = Invocation(agent.clone());
    while let Some(entry) = queue.pop_front() {
        agent.borrow_mut().entries -= 1;
        loop {
            // Adoption can occur during a callback while this invocation queue
            // is on the Rust stack. Resolve the existing adoption map instead
            // of retaining a second mutable cursor/index per queue entry.
            let (realm, node) = entry.realm.resolve_adopted_node(entry.key.node);
            let key = RealmNode::new(&realm, node);
            entry.hub.refresh_pending_form_associations();
            let reaction = entry.hub.state.borrow_mut().reactions.pop_for(key);
            let Some(reaction) = reaction else { break; };
            let (limited, report) = {
                let mut state = agent.borrow_mut();
                state.invoked += 1;
                let limited = state.invoked > MAX_REACTIONS || state.invoking > 256;
                let report = limited && !state.invocation_failed;
                if limited { state.invocation_failed = true; }
                (limited, report)
            };
            if limited {
                entry.hub.state.borrow_mut().reactions.retain(|reaction| reaction.node() != key);
                if report {
                    let exception = OpError::new("QuotaExceededError", "custom element reaction invocation limit").to_value(ctx);
                    crate::error_reporting::report_exception(ctx, exception);
                }
                break;
            }
            entry.hub.invoke_reaction(ctx, reaction);
        }
    }
}

pub(crate) fn end_reaction_scope(ctx: &mut Ctx) {
    let agent = agent_reactions(ctx);
    let queue = agent.borrow_mut().stack.pop();
    if let Some(queue) = queue { invoke_element_queue(ctx, &agent, queue); }
    let mut state = agent.borrow_mut();
    if state.stack.is_empty() && state.stack.capacity() > 256 { state.stack = Vec::new(); }
}

fn abort_reaction_scope(ctx: &mut Ctx) {
    let agent = agent_reactions(ctx);
    let queue = agent.borrow_mut().stack.pop();
    if let Some(queue) = queue {
        for entry in queue {
            agent.borrow_mut().entries -= 1;
            let (realm, node) = entry.realm.resolve_adopted_node(entry.key.node);
            let key = RealmNode::new(&realm, node);
            entry.hub.state.borrow_mut().reactions.retain(|reaction| reaction.node() != key);
        }
    }
}

fn state_agent_schedule(owner: &std::rc::Weak<RefCell<HubState>>) -> bool {
    let Some(owner) = owner.upgrade() else { return false; };
    let agent = owner.borrow().reactions.agent.as_ref().and_then(std::rc::Weak::upgrade);
    agent.is_none_or(|agent| agent.borrow_mut().schedule_backup())
}

struct ActiveConstructorScope(Rc<RefCell<AgentReactions>>);

impl Drop for ActiveConstructorScope {
    fn drop(&mut self) { self.0.borrow_mut().active_constructors.pop(); }
}

struct PendingConstruction(Rc<RefCell<HubState>>);

impl Drop for PendingConstruction {
    fn drop(&mut self) { self.0.borrow_mut().pending_upgrade.pop(); }
}

fn active_constructor_scope(hub: &CustomElementHub, constructor: &Value) -> OpResult<Option<ActiveConstructorScope>> {
    let agent = hub.state.borrow().reactions.agent.as_ref().and_then(std::rc::Weak::upgrade);
    let Some(agent) = agent else { return Ok(None); };
    let owner = hub.state.borrow().reactions.owner.clone();
    {
        let mut state = agent.borrow_mut();
        if state.active_constructors.len() >= MAX_REACTION_SCOPES {
            return Err(OpError::new("QuotaExceededError", "active custom element constructor limit"));
        }
        state.active_constructors.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "active constructor allocation"))?;
        state.active_constructors.push((constructor.clone(), owner));
    }
    Ok(Some(ActiveConstructorScope(agent)))
}

fn invoke_lifecycle_callback(ctx: &mut Ctx, callback: Value, target: Value, args: &[Value]) -> bool {
    let realm = JsFunction::from_value(callback.clone()).and_then(|function| ctx.function_host_realm(&function).ok());
    let invoke = |ctx: &mut Ctx| match ctx.invoke(callback, target, args) {
        Ok(_) => true,
        Err(error) => {crate::error_reporting::report_exception(ctx, error); false}
    };
    if let Some(realm) = realm {
        match ctx.with_host_realm(&realm, invoke) {
            Ok(success) => success,
            Err(_) => {
                let error = OpError::new("InvalidStateError", "custom element callback realm is unavailable").to_value(ctx);
                crate::error_reporting::report_exception(ctx, error);
                false
            }
        }
    } else { invoke(ctx) }
}

#[derive(Default)]
struct ReactionQueue {
    elements: VecDeque<RealmNode>,
    queues: HashMap<RealmNode, VecDeque<Reaction>>,
    current: Option<RealmNode>,
    count: usize,
    allocation_failed: bool,
    agent: Option<std::rc::Weak<RefCell<AgentReactions>>>,
    owner: std::rc::Weak<CustomElementHub>,
}

impl ReactionQueue {
    fn len(&self) -> usize { self.count }

    fn push_back(&mut self, reaction: Reaction) {
        if self.count >= 100_000 || self.elements.len() >= 100_000 {
            self.allocation_failed = true; return;
        }
        let key = reaction.node();
        if self.agent.is_none() && self.elements.try_reserve(1).is_err() { self.allocation_failed = true; return; }
        if !self.queues.contains_key(&key) {
            if self.queues.try_reserve(1).is_err() {
                self.allocation_failed = true;
                return;
            }
            self.queues.insert(key, VecDeque::new());
        }
        let queue = self.queues.get_mut(&key).expect("reaction queue installed");
        if queue.try_reserve(1).is_err() {
            self.allocation_failed = true;
            if queue.is_empty() { self.queues.remove(&key); }
            return;
        }
        queue.push_back(reaction);
        if let Some(agent) = self.agent.as_ref().and_then(std::rc::Weak::upgrade) {
            let admitted = self.owner.upgrade().is_some_and(|hub| agent.borrow_mut().enqueue(hub, key));
            if !admitted {
                queue.pop_back();
                if queue.is_empty() { self.queues.remove(&key); }
                self.allocation_failed = true;
                return;
            }
            self.count += 1;
            return;
        }
        // The element queue deliberately permits repeated entries. An older
        // entry can consume new reactions enqueued by an intervening element.
        self.elements.push_back(key);
        self.count += 1;
    }

    fn pop_front(&mut self) -> Option<Reaction> {
        loop {
            let key = match self.current {
                Some(key) => key,
                None => {
                    let Some(key) = self.elements.pop_front() else {
                        if self.elements.capacity() > 256 { self.elements = VecDeque::new(); }
                        if self.queues.capacity() > 256 { self.queues = HashMap::new(); }
                        return None;
                    };
                    self.current = Some(key);
                    key
                }
            };
            if let Some(reaction) = self.queues.get_mut(&key).and_then(VecDeque::pop_front) {
                self.count -= 1;
                return Some(reaction);
            }
            self.queues.remove(&key);
            self.current = None;
        }
    }

    fn pop_for(&mut self, key: RealmNode) -> Option<Reaction> {
        let reaction = self.queues.get_mut(&key).and_then(VecDeque::pop_front);
        if reaction.is_some() {
            self.count -= 1;
            if let Some(agent) = self.agent.as_ref().and_then(std::rc::Weak::upgrade) { agent.borrow_mut().reactions -= 1; }
        } else {
            self.queues.remove(&key);
            if self.queues.is_empty() && self.queues.capacity() > 256 { self.queues = HashMap::new(); }
        }
        reaction
    }

    fn retain(&mut self, mut keep: impl FnMut(&Reaction) -> bool) {
        let mut count = 0;
        self.queues.retain(|_, queue| {
            queue.retain(|reaction| keep(reaction));
            count += queue.len();
            !queue.is_empty()
        });
        if let Some(agent) = self.agent.as_ref().and_then(std::rc::Weak::upgrade) { agent.borrow_mut().reactions -= self.count - count; }
        self.count = count;
        self.elements.retain(|key| self.queues.contains_key(key));
    }

    fn rebase(&mut self, old: RealmNode, new: RealmNode) {
        let Some(mut queue) = self.queues.remove(&old) else { return; };
        for reaction in &mut queue { reaction.set_node(new); }
        if let Some(existing) = self.queues.get_mut(&new) {
            if existing.try_reserve(queue.len()).is_err() {
                self.count -= queue.len();
                if let Some(agent) = self.agent.as_ref().and_then(std::rc::Weak::upgrade) { agent.borrow_mut().reactions -= queue.len(); }
                self.allocation_failed = true;
            } else { existing.append(&mut queue); }
            self.elements.retain(|key| *key != old);
        } else {
            self.queues.insert(new, queue);
            for key in &mut self.elements { if *key == old { *key = new; } }
        }
        if self.current == Some(old) { self.current = Some(new); }
    }
}

#[derive(Default)]
struct HubState {
    definitions: HashMap<String, Definition>,
    definition_running: bool,
    waiters: HashMap<String, Deferred>,
    pending_upgrade: Vec<PendingUpgrade>,
    upgraded: HashMap<RealmNode, String>,
    failed: HashSet<RealmNode>,
    fresh_fallbacks: HashSet<RealmNode>,
    internals_shadow_roots: HashSet<RealmNode>,
    connected: HashMap<RealmNode, bool>,
    form_states: HashMap<RealmNode, (Option<NodeId>,bool)>,
    form_refresh_pending: bool,
    reactions: ReactionQueue,
}

impl HubState {
    fn callbacks_enabled(&self, key: RealmNode) -> bool {
        !self.pending_upgrade.iter().any(|pending| pending.node == key.node &&
            Rc::as_ptr(&pending.realm) as usize == key.realm && !pending.consumed_by_super)
    }
}

struct DefinitionRunning(Rc<RefCell<HubState>>);

impl Drop for DefinitionRunning {
    fn drop(&mut self) {
        self.0.borrow_mut().definition_running = false;
    }
}

fn collect_definition_callbacks(ctx: &mut Ctx, prototype: &Value,
    names: &[&'static str], callbacks: &mut HashMap<&'static str, Value>) -> OpResult<()> {
    for &name in names {
        let callback = ctx.member_get(prototype, name).map_err(OpError::thrown)?;
        if !matches!(callback, Value::Undefined) && !callback.is_callable() {
            return Err(OpError::type_error(format!("{name} must be callable")));
        }
        if callback.is_callable() {
            callbacks.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element callback limit"))?;
            callbacks.insert(name, callback);
        }
    }
    Ok(())
}

fn definition_for_element<'a>(state: &'a HubState, local: &str, is_value: Option<&str>) -> Option<&'a Definition> {
    state.definitions.get(local).filter(|definition| definition.extends.is_none())
        .or_else(|| is_value.and_then(|name| state.definitions.get(name))
            .filter(|definition| definition.extends.as_deref() == Some(local)))
}

fn enqueue_connection_subtree(state: &mut HubState, document: &lumen_html::Document,
    realm: &Rc<DomRealm>, root: NodeId, connected: bool) {
    let mut cursor = Some(root);
    let mut remaining = document.node_count();
    while let Some(id) = cursor {
        if remaining == 0 { break; }
        remaining -= 1;
        let key = RealmNode::new(realm, id);
        if state.upgraded.contains_key(&key) && state.callbacks_enabled(key) &&
            state.connected.get(&key).copied().unwrap_or(false) != connected {
            state.connected.insert(key, connected);
            let callback = state.upgraded.get(&key).and_then(|name| state.definitions.get(name))
                .and_then(|definition| definition.callbacks.get(if connected {"connectedCallback"} else {"disconnectedCallback"})).cloned();
            if let Some(callback) = callback {
                state.reactions.push_back(if connected {Reaction::Connected(key, callback)} else {Reaction::Disconnected(key, callback)});
            }
        }
        cursor = selector::next_shadow_including_descendant(document, root, id).ok().flatten();
    }
}

/// One rare policy per native document, with weak registries rather than a
/// callback chain or state attached to every ordinary element.
pub(crate) struct ShadowPolicy {
    primary: RefCell<std::rc::Weak<RefCell<HubState>>>,
    registries: RefCell<Vec<std::rc::Weak<RefCell<HubState>>>>,
    associations: RefCell<HashMap<NodeId, Option<std::rc::Weak<CustomElementHub>>>>,
}

impl ShadowPolicy {
    fn allows(&self, document: &lumen_html::Document, realm: &Rc<DomRealm>, node: NodeId) -> bool {
        let key = RealmNode::new(realm, node);
        for registry in self.registries.borrow().iter().filter_map(std::rc::Weak::upgrade) {
            let state = registry.borrow();
            if let Some(name) = state.upgraded.get(&key) {
                return state.definitions.get(name).is_none_or(|definition| !definition.disabled_shadow);
            }
        }
        if document.node_document(node).ok().is_some_and(|owner| document.is_template_owner_document(owner)) {
            return true;
        }
        if let Some(registry)=self.associations.borrow().get(&node) {
            let Some(hub)=registry.as_ref().and_then(std::rc::Weak::upgrade) else {return true;};
            let state=hub.state.borrow();let Ok((_,local))=document.element_name_parts(node) else {return false;};
            let is_value=document.custom_element_is_value(node).ok().flatten();
            return definition_for_element(&state,local,is_value).is_none_or(|definition|!definition.disabled_shadow);
        }
        let Some(primary) = self.primary.borrow().upgrade() else { return true; };
        let state = primary.borrow();
        let Ok((_, local)) = document.element_name_parts(node) else { return false; };
        let is_value = document.custom_element_is_value(node).ok().flatten();
        definition_for_element(&state, local, is_value).is_none_or(|definition| !definition.disabled_shadow)
    }
}

struct PendingUpgrade {
    realm: Rc<DomRealm>,
    node: NodeId,
    definition: String,
    consumed_by_super: bool,
}

#[derive(Clone)]
pub(crate) struct CustomElementHub {
    realm: Rc<DomRealm>,
    // Hubs are cloned into the registry and reaction-delivery native instances. Keep realm
    // registration shared so a realm attached while adopting nodes is visible to the delivery
    // clone that later resolves callback receivers.
    attached_realms: Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>,
    scoped_documents: Rc<RefCell<Vec<(std::rc::Weak<DomRealm>, NodeId)>>>,
    state: Rc<RefCell<HubState>>,
    jobs: Rc<RefCell<Vec<Value>>>,
    delivery: Rc<RefCell<Option<Value>>>,
    scheduled: Rc<Cell<bool>>,
    scoped: bool,
    registry: Rc<RefCell<Option<WeakValue>>>,
}

#[lumen_bind::class(name = "CustomElementRegistry", hint(js(webidl)))]
pub(crate) struct DomCustomElementRegistry {
    hub: Rc<CustomElementHub>,
}
impl DomCustomElementRegistry {
    pub(crate) fn hub_for_import(&self)->Rc<CustomElementHub> {self.hub.clone()}
}

// The JavaScript registry owns its definitions. Realm service lookup and the
// queued delivery handle are weak, so an unreachable registry/document can be
// collected while an author-retained registry still keeps its callbacks alive.
impl lumen::embed::NativeIdentityOwner for DomCustomElementRegistry {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        let state = self.hub.state.borrow();
        for definition in state.definitions.values() {
            visit(&definition.constructor);
            for callback in definition.callbacks.values() { visit(callback); }
        }
        for waiter in state.waiters.values() { visit(waiter.promise_value()); }
        for queue in state.reactions.queues.values() {
            for reaction in queue {
                match reaction {
                    Reaction::Upgrade(_) => {},
                    Reaction::Connected(_, callback) | Reaction::Disconnected(_, callback)
                    | Reaction::Attribute(_, callback, ..) | Reaction::FormAssociated(_, callback, ..) | Reaction::FormDisabled(_, callback, ..) | Reaction::FormReset(_, callback) => visit(callback),
                    Reaction::FormRestore(_,callback,value)=>{visit(callback);visit(value);},
                    Reaction::Adopted(_,callback,old,new)=>{visit(callback);visit(old);visit(new);},
                    Reaction::AdoptedPending(_,callback,_,_)=>visit(callback),
                    Reaction::MoveFallback(_, disconnected, connected) => {
                        if let Some(callback) = disconnected { visit(callback); }
                        if let Some(callback) = connected { visit(callback); }
                    },
                }
            }
        }
        // Scoped hubs share the global delivery cell: enumerate that physical
        // Value only through its global registry, not once per scoped registry.
        if !self.hub.scoped {
            if let Some(delivery) = self.hub.delivery.borrow().as_ref() { visit(delivery); }
        }
    }
}

struct RegistryConstructorResult(DomCustomElementRegistry);
impl CtorRet<JsHost, DomCustomElementRegistry> for RegistryConstructorResult {
    fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
        let hub=self.0.hub.clone();
        let value = <JsHost as Host>::construct(cx, self.0)?;
        <JsHost as Host>::with_ctx(cx, |ctx| {
            ctx.set_native_identity_owner::<DomCustomElementRegistry>(&value)
                .expect("CustomElementRegistry native brand");
            *hub.registry.borrow_mut()=ctx.weak_value(&value);
        });
        Ok(value)
    }
}

struct FormValueArgument(Option<Value>);
impl<'a> lumen_bind::FromArg<'a,JsHost> for FormValueArgument {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value> {
        cx.before_js();cx.with_ctx(|ctx| {
            if matches!(value,Value::Null|Value::Undefined){return Ok(Self(None));}
            if lumen_host::blob::is_file(ctx,value)||lumen_host::blob::is_form_data(ctx,value){return Ok(Self(Some(value.clone())));}
            let text=lumen_host::webidl::usv_string(ctx,value)?;
            Ok(Self(Some(Value::from_string(text))))
        })
    }
}
struct ValidityFlags(lumen_html::forms::ValidityState);
impl<'a> lumen_bind::FromArg<'a,JsHost> for ValidityFlags {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        cx.before_js();cx.with_ctx(|ctx| {
            let mut flags=lumen_html::forms::ValidityState::default();
            if matches!(value,Value::Null|Value::Undefined){return Ok(Self(flags));}
            if !matches!(value,Value::Obj(_)){return Err(ctx.make_error("TypeError","validity flags must be a dictionary"));}
            // Web IDL dictionary members are converted in lexicographic order.
            for (name,slot) in [("badInput",&mut flags.bad_input),("customError",&mut flags.custom_error),
                ("patternMismatch",&mut flags.pattern_mismatch),("rangeOverflow",&mut flags.range_overflow),
                ("rangeUnderflow",&mut flags.range_underflow),("stepMismatch",&mut flags.step_mismatch),
                ("tooLong",&mut flags.too_long),("tooShort",&mut flags.too_short),
                ("typeMismatch",&mut flags.type_mismatch),("valueMissing",&mut flags.value_missing)] {
                let value=ctx.member_get(value,name)?;*slot=ctx.to_boolean(&value);
            }
            Ok(Self(flags))
        })
    }
    fn from_missing(_: &'a lumen::embed::ArgCx<'_>,_:lumen_bind::Slot)->Result<Self,Value>{Ok(Self(lumen_html::forms::ValidityState::default()))}
}
struct ValidationMessage(String);
impl<'a> lumen_bind::FromArg<'a,JsHost> for ValidationMessage {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        if matches!(value,Value::Undefined){return Ok(Self(String::new()));}
        cx.before_js();cx.with_ctx(|ctx|ctx.coerce_string(value).map(|text|Self(text.to_string())))
    }
    fn from_missing(_: &'a lumen::embed::ArgCx<'_>,_:lumen_bind::Slot)->Result<Self,Value>{Ok(Self(String::new()))}
}
struct OptionalValidationAnchor(Option<Value>);
impl<'a> lumen_bind::FromArg<'a,JsHost> for OptionalValidationAnchor {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        if matches!(value,Value::Undefined){return Ok(Self(None));}
        cx.with_ctx(|ctx|ctx.with_instance::<DomHtmlElement,_>(value,|_|()).map_err(|error|error.to_value(ctx)))?;
        Ok(Self(Some(value.clone())))
    }
    fn from_missing(_: &'a lumen::embed::ArgCx<'_>,_:lumen_bind::Slot)->Result<Self,Value>{Ok(Self(None))}
}

pub(crate) struct AriaStringArgument(pub(crate) Option<String>);
impl<'a> lumen_bind::FromArg<'a,JsHost> for AriaStringArgument {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        if matches!(value,Value::Null|Value::Undefined){return Ok(Self(None));}
        cx.before_js();cx.with_ctx(|ctx|ctx.coerce_string(value).map(|value|Self(Some(value.to_string()))))
    }
}
struct AriaElementArgument(Option<Vec<Value>>);
impl<'a> lumen_bind::FromArg<'a,JsHost> for AriaElementArgument {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        if matches!(value,Value::Null|Value::Undefined){return Ok(Self(None));}
        cx.with_ctx(|ctx|ctx.with_instance::<DomElement,_>(value,|_|()).map_err(|error|error.to_value(ctx)))?;
        Ok(Self(Some(vec![value.clone()])))
    }
}
struct AriaElementsArgument(Option<Vec<Value>>);
impl<'a> lumen_bind::FromArg<'a,JsHost> for AriaElementsArgument {
    fn from_arg(cx:&'a lumen::embed::ArgCx<'_>,value:&'a Value,_:lumen_bind::Slot)->Result<Self,Value>{
        if matches!(value,Value::Null|Value::Undefined){return Ok(Self(None));}
        cx.before_js();cx.with_ctx(|ctx| {
            let values=ctx.convert_iterable(value,MAX_REGISTRY_ENTRIES,|ctx,value| {
                ctx.with_instance::<DomElement,_>(&value,|_|())?;Ok(value)
            }).map_err(|error|error.to_value(ctx))?;
            Ok(Self(Some(values)))
        })
    }
}
macro_rules! aria_properties {
    ($d:tt; strings[$(($getter:ident,$setter:ident,$idl:literal,$attribute:literal)),* $(,)?]; references[$(($rg:ident,$rs:ident,$rid:literal,$ra:literal,$argument:ty)),* $(,)?]; internals{$($internals:tt)*})=>{
        #[lumen_bind::methods]
        impl DomElementInternals {
            $(#[getter(name=$idl)] fn $getter(&self)->Value {self.aria_value($attribute)}
              #[setter(name=$idl)] fn $setter(&self,value:AriaStringArgument)->OpResult<()> {self.set_aria_value($attribute,value.0.map(Value::from_string))})*
            $(#[getter(name=$rid)] fn $rg(&self,ctx:&mut Ctx)->OpResult<Value> {self.aria_reference_value(ctx,$ra,stringify!($argument)=="AriaElementsArgument")}
              #[setter(name=$rid)] fn $rs(&self,ctx:&mut Ctx,value:$argument)->OpResult<()> {self.set_aria_reference(ctx,$ra,value.0)})*
            $($internals)*
        }
        macro_rules! bind_element_aria {
            ($d($d base:tt)*)=>{
                #[lumen_bind::methods]
                impl DomElement {
                    $(#[getter(name=$idl)] fn $getter(&self)->OpResult<Nullable<String>> {self.base.get_attribute($attribute)}
                      #[setter(name=$idl,hint(js(ce_reactions)))] fn $setter(&self,ctx:&mut Ctx,value:custom_elements::AriaStringArgument)->OpResult<()> {
                        if let Some(value)=value.0 {self.base.set_attribute(ctx,$attribute,&value)}else{self.base.remove_attribute(ctx,$attribute)}
                      })*
                    $d($d base)*
                }
            }
        }
        pub(crate) use bind_element_aria;
    }
}
// ARIAMixin, WAI-ARIA 1.3 §10.1: one typed declaration drives both reflection targets.
aria_properties! { $; strings[
    (role,set_role,"role","role"),
    (aria_atomic,set_aria_atomic,"ariaAtomic","aria-atomic"),
    (aria_auto_complete,set_aria_auto_complete,"ariaAutoComplete","aria-autocomplete"),
    (aria_braille_label,set_aria_braille_label,"ariaBrailleLabel","aria-braillelabel"),
    (aria_braille_role_description,set_aria_braille_role_description,"ariaBrailleRoleDescription","aria-brailleroledescription"),
    (aria_busy,set_aria_busy,"ariaBusy","aria-busy"),
    (aria_checked,set_aria_checked,"ariaChecked","aria-checked"),
    (aria_col_count,set_aria_col_count,"ariaColCount","aria-colcount"),
    (aria_col_index,set_aria_col_index,"ariaColIndex","aria-colindex"),
    (aria_col_index_text,set_aria_col_index_text,"ariaColIndexText","aria-colindextext"),
    (aria_col_span,set_aria_col_span,"ariaColSpan","aria-colspan"),
    (aria_current,set_aria_current,"ariaCurrent","aria-current"),
    (aria_description,set_aria_description,"ariaDescription","aria-description"),
    (aria_disabled,set_aria_disabled,"ariaDisabled","aria-disabled"),
    (aria_expanded,set_aria_expanded,"ariaExpanded","aria-expanded"),
    (aria_has_popup,set_aria_has_popup,"ariaHasPopup","aria-haspopup"),
    (aria_hidden,set_aria_hidden,"ariaHidden","aria-hidden"),
    (aria_invalid,set_aria_invalid,"ariaInvalid","aria-invalid"),
    (aria_key_shortcuts,set_aria_key_shortcuts,"ariaKeyShortcuts","aria-keyshortcuts"),
    (aria_label,set_aria_label,"ariaLabel","aria-label"),
    (aria_level,set_aria_level,"ariaLevel","aria-level"),
    (aria_live,set_aria_live,"ariaLive","aria-live"),
    (aria_modal,set_aria_modal,"ariaModal","aria-modal"),
    (aria_multi_line,set_aria_multi_line,"ariaMultiLine","aria-multiline"),
    (aria_multi_selectable,set_aria_multi_selectable,"ariaMultiSelectable","aria-multiselectable"),
    (aria_orientation,set_aria_orientation,"ariaOrientation","aria-orientation"),
    (aria_placeholder,set_aria_placeholder,"ariaPlaceholder","aria-placeholder"),
    (aria_pos_in_set,set_aria_pos_in_set,"ariaPosInSet","aria-posinset"),
    (aria_pressed,set_aria_pressed,"ariaPressed","aria-pressed"),
    (aria_read_only,set_aria_read_only,"ariaReadOnly","aria-readonly"),
    (aria_relevant,set_aria_relevant,"ariaRelevant","aria-relevant"),
    (aria_required,set_aria_required,"ariaRequired","aria-required"),
    (aria_role_description,set_aria_role_description,"ariaRoleDescription","aria-roledescription"),
    (aria_row_count,set_aria_row_count,"ariaRowCount","aria-rowcount"),
    (aria_row_index,set_aria_row_index,"ariaRowIndex","aria-rowindex"),
    (aria_row_index_text,set_aria_row_index_text,"ariaRowIndexText","aria-rowindextext"),
    (aria_row_span,set_aria_row_span,"ariaRowSpan","aria-rowspan"),
    (aria_selected,set_aria_selected,"ariaSelected","aria-selected"),
    (aria_set_size,set_aria_set_size,"ariaSetSize","aria-setsize"),
    (aria_sort,set_aria_sort,"ariaSort","aria-sort"),
    (aria_value_max,set_aria_value_max,"ariaValueMax","aria-valuemax"),
    (aria_value_min,set_aria_value_min,"ariaValueMin","aria-valuemin"),
    (aria_value_now,set_aria_value_now,"ariaValueNow","aria-valuenow"),
    (aria_value_text,set_aria_value_text,"ariaValueText","aria-valuetext"),
]; references[
    (aria_active_descendant_element,set_aria_active_descendant_element,"ariaActiveDescendantElement","aria-activedescendant",AriaElementArgument),
    (aria_controls_elements,set_aria_controls_elements,"ariaControlsElements","aria-controls",AriaElementsArgument),
    (aria_described_by_elements,set_aria_described_by_elements,"ariaDescribedByElements","aria-describedby",AriaElementsArgument),
    (aria_details_elements,set_aria_details_elements,"ariaDetailsElements","aria-details",AriaElementsArgument),
    (aria_error_message_elements,set_aria_error_message_elements,"ariaErrorMessageElements","aria-errormessage",AriaElementsArgument),
    (aria_flow_to_elements,set_aria_flow_to_elements,"ariaFlowToElements","aria-flowto",AriaElementsArgument),
    (aria_labelled_by_elements,set_aria_labelled_by_elements,"ariaLabelledByElements","aria-labelledby",AriaElementsArgument),
    (aria_owns_elements,set_aria_owns_elements,"ariaOwnsElements","aria-owns",AriaElementsArgument),
]; internals{
#[getter]
fn states(&self,ctx:&mut Ctx)->OpResult<Value> {
    if let Some(value)=self.states.borrow().as_ref().and_then(WeakValue::upgrade){return Ok(value);}
    let data=self.form.borrow_mut().states.get_or_insert_with(||Rc::new(RefCell::new(lumen::embed::DomStringSet::default()))).clone();
    let value=ctx.new_instance(DomCustomStateSet{target:self.target.clone(),realm:self.realm.clone(),node:self.node,data});
    ctx.set_native_identity_owner::<DomCustomStateSet>(&value)?;
    self.states.replace(ctx.weak_value(&value));Ok(value)
}
fn set_form_value(&self,ctx:&mut Ctx,value:FormValueArgument,state:lumen_bind::Passed<FormValueArgument>)->OpResult<()> {
    let (realm,_)=self.form_target()?;
    let snapshot=|ctx:&mut Ctx,value:Option<Value>|->OpResult<Option<Value>> {
        value.map(|value|if lumen_host::blob::is_form_data(ctx,&value){lumen_host::blob::clone_form_data(ctx,&value)}else{Ok(value)}).transpose()
    };
    let submission=snapshot(ctx,value.0)?;
    let restore=if let Some(state)=state.0 {snapshot(ctx,state.0)?}else{submission.clone()};
    let mut form=self.form.borrow_mut();form.submission=submission;form.restore=restore;drop(form);
    realm.forms.borrow_mut().changed_custom_form_state();Ok(())
}
#[getter]
fn form(&self,ctx:&mut Ctx)->OpResult<Value> {
    let (realm,node)=self.form_target()?;Ok(forms::form_owner_value(ctx,&realm,node))
}
fn set_validity(&self,ctx:&mut Ctx,#[default(ValidityFlags(Default::default()))] flags:ValidityFlags,#[default(ValidationMessage(String::new()))] message:ValidationMessage,#[default(OptionalValidationAnchor(None))] anchor:OptionalValidationAnchor)->OpResult<()> {
    let (realm,node)=self.form_target()?;
    let message=message.0;
    if !flags.0.valid() && message.is_empty(){return Err(OpError::type_error("invalid custom element requires a nonempty validation message"));}
    let message=message.replace("\r\n","\n").replace('\r',"\n");
    {let mut form=self.form.borrow_mut();form.validity=flags.0;form.message=if flags.0.valid(){String::new()}else{message};}
    realm.forms.borrow_mut().changed_custom_form_state();
    let anchor=anchor.0.unwrap_or_else(||self.target.clone());
    let (owner,id)=ctx.with_instance::<DomNode,_>(&anchor,|node|node.realm.resolve_adopted_node(node.id))?;
    let inside=Rc::ptr_eq(&realm,&owner)&&{
        let session=realm.session.borrow();let document=session.document();let mut cursor=Some(id);let mut found=false;
        while let Some(id)=cursor {if id==node{found=true;break;}cursor=document.shadow_including_parent(id).map_err(dom_error)?;}found
    };
    if !inside{return Err(OpError::new("NotFoundError","validation anchor is outside target's shadow-including subtree"));}
    self.form.borrow_mut().anchor=Some(anchor);Ok(())
}
#[getter]
fn will_validate(&self)->OpResult<bool>{let (realm,node)=self.form_target()?;Ok(forms::will_validate(&realm,node))}
#[getter]
fn validity(&self,ctx:&mut Ctx)->OpResult<Value>{
    self.form_target()?;
    if let Some(value)=self.validity.borrow().as_ref().and_then(WeakValue::upgrade){return Ok(value);}
    let value=forms::validity_object(ctx,self.target.clone())?;*self.validity.borrow_mut()=ctx.weak_value(&value);Ok(value)
}
#[getter]
fn validation_message(&self)->OpResult<String>{
    let (realm,node)=self.form_target()?;
    if !forms::will_validate(&realm,node) || self.form.borrow().validity.valid(){return Ok(String::new());}
    Ok(self.form.borrow().message.clone())
}
fn check_validity(&self,ctx:&mut Ctx)->OpResult<bool>{let (realm,node)=self.form_target()?;forms::check_validity(ctx,&realm,node,&realm.forms)}
fn report_validity(&self,ctx:&mut Ctx)->OpResult<bool>{let (realm,node)=self.form_target()?;forms::report_custom_validity(ctx,&realm,node,&self.form)}
#[getter]
fn labels(&self,ctx:&mut Ctx)->OpResult<Value>{
    let (realm,node)=self.form_target()?;
    if let Some(value)=self.labels.borrow().as_ref().and_then(WeakValue::upgrade){return Ok(value);}
    let value=ctx.new_instance(DomNodeList::label_associations(realm,node,self.target.clone()));
    ctx.set_native_identity_owner::<DomNodeList>(&value)?;
    *self.labels.borrow_mut()=ctx.weak_value(&value);Ok(value)
}
    #[getter]
    fn shadow_root(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let (realm, node) = self.realm.resolve_adopted_node(self.node);
        let root = {
            let session = realm.session.borrow();
            let document = session.document();
            document.shadow_root(node).map_err(dom_error)?.filter(|root| {
                document.shadow_options(*root).ok().flatten().is_some_and(|options| options.declarative)
                    || self.state.borrow().internals_shadow_roots.contains(&RealmNode::new(&realm, *root))
            })
        };
        Ok(realm.wrap_option(ctx, root))
    }

} }

struct AriaReference {
    explicit:Vec<WeakValue>,
    cached:Option<(Vec<WeakValue>,Value)>,
}
enum AriaContent {
    String(String),
    Reference(AriaReference),
}
impl AriaContent {
    fn trace_values(&self,visit:&mut dyn FnMut(&Value)) {
        if let Self::Reference(reference)=self {if let Some((_,cached))=&reference.cached{visit(cached);}}
    }
}

const ATTACHED_INTERNALS_SLOT: &str = "#lumen_attached_internals\u{1}element";

#[lumen_bind::class(name = "ElementInternals", hint(js(webidl)))]
pub(crate) struct DomElementInternals {
    target: Value,
    realm: Rc<DomRealm>,
    node: NodeId,
    state: Rc<RefCell<HubState>>,
    form:Rc<RefCell<forms::CustomFormState>>,
    validity:RefCell<Option<WeakValue>>,
    labels:RefCell<Option<WeakValue>>,
    states:RefCell<Option<WeakValue>>,
    aria:RefCell<Option<HashMap<&'static str,AriaContent>>>,
}
impl lumen::embed::NativeIdentityOwner for DomElementInternals {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) { visit(&self.target); self.form.borrow().trace_values(visit);if let Some(aria)=self.aria.borrow().as_ref(){for value in aria.values(){value.trace_values(visit);}} }
}
impl DomElementInternals {
    fn aria_value(&self,key:&str)->Value {
        self.aria.borrow().as_ref().and_then(|map|map.get(key)).map_or(Value::Null,|value|match value {AriaContent::String(value)=>Value::from_string(value.clone()),AriaContent::Reference(_)=>Value::from_string(String::new())})
    }
    fn set_aria_value(&self,key:&'static str,value:Option<Value>)->OpResult<()> {
        let mut aria=self.aria.borrow_mut();
        if let Some(value)=value {
            let map=aria.get_or_insert_with(HashMap::new);
            map.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","ARIA map allocation failed"))?;
            let Value::Str(value)=value else{return Err(OpError::type_error("ARIA string value required"));};
            map.insert(key,AriaContent::String(value.as_str().to_owned()));
        }else if let Some(map)=aria.as_mut(){map.remove(key);}
        Ok(())
    }
fn set_aria_reference(&self,ctx:&mut Ctx,key:&'static str,values:Option<Vec<Value>>)->OpResult<()> {
    let mut aria=self.aria.borrow_mut();
    if let Some(values)=values {
        let map=aria.get_or_insert_with(HashMap::new);
        map.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","ARIA map allocation failed"))?;
        map.insert(key,AriaContent::Reference(AriaReference{explicit:values.iter().filter_map(|value|ctx.weak_value(value)).collect(),cached:None}));
    }else if let Some(map)=aria.as_mut(){map.remove(key);}
    Ok(())
}
fn aria_reference_value(&self,ctx:&mut Ctx,key:&str,multiple:bool)->OpResult<Value> {
    let (realm,node)=self.realm.resolve_adopted_node(self.node);
    let explicit={let aria=self.aria.borrow();let Some(AriaContent::Reference(reference))=aria.as_ref().and_then(|map|map.get(key)) else{return Ok(Value::Null);};reference.explicit.clone()};
    let mut values=Vec::new();
    values.try_reserve(explicit.len()).map_err(|_|OpError::new("QuotaExceededError","ARIA result allocation failed"))?;
    for value in explicit.iter().filter_map(WeakValue::upgrade) {
        let (candidate_realm,candidate)=ctx.with_instance::<DomElement,_>(&value,|element|element.base.realm.resolve_adopted_node(element.base.id))?;
        if !Rc::ptr_eq(&realm,&candidate_realm){continue;}
        let session=realm.session.borrow();let document=session.document();
        let mut ancestor=document.shadow_including_parent(node).map_err(dom_error)?;let mut permitted=false;
        while let Some(id)=ancestor {
            let mut cursor=document.parent(candidate).map_err(dom_error)?;
            while let Some(current)=cursor {if current==id{permitted=true;break;}cursor=document.parent(current).map_err(dom_error)?;}
            if permitted{break;}ancestor=document.shadow_including_parent(id).map_err(dom_error)?;
        }
        if permitted{values.push(value);}
    }
    if !multiple{return Ok(values.into_iter().next().unwrap_or(Value::Null));}
    let cached={let aria=self.aria.borrow();match aria.as_ref().and_then(|map|map.get(key)){Some(AriaContent::Reference(reference))=>reference.cached.clone(),_=>None}};
    if let Some((previous,cached))=cached {
        if previous.len()==values.len() && previous.iter().zip(&values).all(|(a,b)|a.upgrade().is_some_and(|a|lumen::embed::object_identity(&a)==lumen::embed::object_identity(b))){return Ok(cached);}
    }
    let identities=values.iter().filter_map(|value|ctx.weak_value(value)).collect();
    let array=JsHost::from_list(ctx,values);ctx.freeze_native_object(&array);
    if let Some(AriaContent::Reference(reference))=self.aria.borrow_mut().as_mut().and_then(|map|map.get_mut(key)){reference.cached=Some((identities,array.clone()));}
    Ok(array)
}
    fn form_target(&self)->OpResult<(Rc<DomRealm>,NodeId)> {
        let (realm,node)=self.realm.resolve_adopted_node(self.node);
        let key=RealmNode::new(&realm,node);
        let state=self.state.borrow();
        let supported=state.upgraded.get(&key).and_then(|name|state.definitions.get(name)).is_some_and(|definition|definition.form_associated && definition.extends.is_none());
        if !supported {return Err(OpError::new("NotSupportedError","target is not a form-associated custom element"));}
        Ok((realm,node))
    }
}

#[lumen_bind::class(name = "CustomStateSet", hint(js(webidl)))]
pub(crate) struct DomCustomStateSet {
    target: Value,
    realm: Rc<DomRealm>,
    node: NodeId,
    data: Rc<RefCell<lumen::embed::DomStringSet>>,
}
impl lumen::embed::NativeIdentityOwner for DomCustomStateSet {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit:&mut dyn FnMut(&Value)) {visit(&self.target);}
}
impl DomCustomStateSet {
    fn changed(&self) {let (realm,_)=self.realm.resolve_adopted_node(self.node);realm.forms.borrow_mut().changed_custom_form_state();}
    fn iterator(&self,ctx:&mut Ctx,owner:Value,entries:bool)->OpResult<Value> {
        let value=ctx.new_instance(DomCustomStateIterator{owner,data:self.data.clone(),cursor:Cell::new(0),done:Cell::new(false),entries});
        ctx.set_native_identity_owner::<DomCustomStateIterator>(&value)?;Ok(value)
    }
}
#[lumen_bind::methods]
impl DomCustomStateSet {
    #[getter]
    fn size(&self)->usize {self.data.borrow().len()}
    #[method(coerce)]
    fn has(&self,value:String)->bool {self.data.borrow().contains(&value)}
    #[method(coerce)]
    fn add(&self,this:lumen_bind::This<Value>,value:String)->Value {
        if self.data.borrow_mut().insert(value) {self.changed();}this.0
    }
    #[method(coerce)]
    fn delete(&self,value:String)->bool {let removed=self.data.borrow_mut().remove(value);if removed{self.changed();}removed}
    fn clear(&self) {let changed=self.data.borrow().len()!=0;self.data.borrow_mut().clear();if changed{self.changed();}}
    #[method(hint(js(also_iterator)))]
    fn values(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,false)}
    fn keys(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,false)}
    fn entries(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>)->OpResult<Value>{self.iterator(ctx,this.0,true)}
    fn for_each(&self,ctx:&mut Ctx,this:lumen_bind::This<Value>,callback:JsFunction,#[default(Value::Undefined)] this_arg:Value)->OpResult<()> {
        let mut cursor=0;
        loop {let value=self.data.borrow().next(&mut cursor);let Some(value)=value else{return Ok(());};
            callback.call(ctx,this_arg.clone(),&[value.clone(),value,this.0.clone()])?;
        }
    }
}
#[lumen_bind::class(name = "CustomStateSetIterator")]
struct DomCustomStateIterator {
    owner:Value,
    data:Rc<RefCell<lumen::embed::DomStringSet>>,
    cursor:Cell<usize>,
    done:Cell<bool>,
    entries:bool,
}
impl lumen::embed::NativeIdentityOwner for DomCustomStateIterator {
    const TRACES_NATIVE_VALUES: bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)){}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)){visit(&self.owner);}
}
#[lumen_bind::methods]
impl DomCustomStateIterator {
    #[proto(iter)]
    fn iter(&self,this:lumen_bind::This<Value>)->Value{this.0}
    #[proto(next)]
    fn next(&self,ctx:&mut Ctx)->OpResult<Option<Value>> {
        if self.done.get(){return Ok(None);}
        let mut cursor=self.cursor.get();let value=self.data.borrow().next(&mut cursor);self.cursor.set(cursor);
        let Some(value)=value else{self.done.set(true);return Ok(None);};
        Ok(Some(if self.entries{JsHost::from_list(ctx,vec![value.clone(),value])}else{value}))
    }
}


pub(crate) fn note_shadow_attachment(realm: &Rc<DomRealm>, host: NodeId, root: NodeId) -> OpResult<()> {
    let Some(hub) = registry_hub(realm, host) else { return Ok(()); };
    let mut state = hub.state.borrow_mut();
    if state.upgraded.contains_key(&RealmNode::new(realm, host)) {
        state.internals_shadow_roots.try_reserve(1)
            .map_err(|_| OpError::new("QuotaExceededError", "element internals shadow allocation"))?;
        state.internals_shadow_roots.insert(RealmNode::new(realm, root));
    }
    Ok(())
}

pub(crate) fn attach_internals(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, target: Value) -> OpResult<Value> {
    let (realm, node) = realm.resolve_adopted_node(node);
    let unsupported = || OpError::new("NotSupportedError", "element cannot attach internals");
    if realm.session.borrow().document().custom_element_is_value(node).map_err(dom_error)?.is_some() {
        return Err(unsupported());
    }
    let hub = registry_hub(&realm, node).ok_or_else(unsupported)?;
    {
        let state = hub.state.borrow();
        let session = realm.session.borrow();
        let document = session.document();
        let (_, local) = document.element_name_parts(node).map_err(dom_error)?;
        let definition = definition_for_element(&state, local, None).ok_or_else(unsupported)?;
        if definition.disabled_internals || !state.upgraded.contains_key(&RealmNode::new(&realm, node)) {
            return Err(unsupported());
        }
    }
    if ctx.native_private_value_slot(&target, ATTACHED_INTERNALS_SLOT).is_some() {
        return Err(unsupported());
    }
    let form=realm.forms.borrow_mut().custom_form_state(node)?;
    let internals = ctx.new_instance(DomElementInternals { target: target.clone(), realm, node, state: hub.state.clone(),form,validity:RefCell::new(None),labels:RefCell::new(None),states:RefCell::new(None),aria:RefCell::new(None) });
    ctx.set_native_identity_owner::<DomElementInternals>(&internals)?;
    ctx.define_native_internal_value_slot(&target, ATTACHED_INTERNALS_SLOT, internals.clone()).map_err(OpError::thrown)?;
    Ok(internals)
}

#[lumen_bind::class(name = "CustomElementReactionDelivery")]
struct ReactionDelivery {
    hub: std::rc::Weak<CustomElementHub>,
}

#[lumen_bind::methods]
impl DomCustomElementRegistry {
    #[constructor]
    fn new(ctx: &mut Ctx) -> OpResult<RegistryConstructorResult> {
        let current=hub_from_ctx(ctx).ok_or_else(||OpError::type_error("custom element document is unavailable"))?;
        let mut hub=CustomElementHub::new(current.realm.clone(),ctx.deferred_microtasks());
        hub.scoped=true;
        // Agent backup delivery drains every registry's queue. Reusing its
        // bound global delivery avoids a scoped hub owning itself through a
        // separately bound ReactionDelivery object.
        hub.delivery=current.delivery.clone();
        hub.scheduled=current.scheduled.clone();
        let hub=Rc::new(hub);
        let agent=agent_reactions(ctx);
        {let mut state=hub.state.borrow_mut();state.reactions.agent=Some(Rc::downgrade(&agent));state.reactions.owner=Rc::downgrade(&hub);}
        hub.observe_mutations()?;
        Ok(RegistryConstructorResult(Self {hub}))
    }
    #[method(coerce, hint(js(ce_reactions)))]
    fn define(
        &self,
        ctx: &mut Ctx,
        name: &str,
        constructor: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        self.hub.define(ctx, name, constructor, options)
    }

    #[method(coerce)]
    fn get(&self, _ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        Ok(self
            .hub
            .state
            .borrow()
            .definitions
            .get(name)
            .map_or(Value::Undefined, |definition| {
                definition.constructor.clone()
            }))
    }

    fn get_name(&self, ctx: &mut Ctx, constructor: JsFunction) -> OpResult<Value> {
        let constructor = constructor.into_value();
        let found = self
            .hub
            .state
            .borrow()
            .definitions
            .values()
            .find(|definition| ctx.values_strict_equal(&definition.constructor, &constructor))
            .map(|definition| Value::from_string(definition.name.clone()))
            .unwrap_or(Value::Null);
        Ok(found)
    }

    #[method(coerce)]
    fn when_defined(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        if !valid_name(name) {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            let error = crate::error_reporting::dom_exception(ctx, "SyntaxError", "invalid custom element name");
            deferred.reject(ctx, error);
            return Ok(promise);
        }
        // Promise resolution can invoke an author-defined constructor.then
        // getter. Release registry storage before entering that callback.
        let constructor = self.hub.state.borrow().definitions.get(name)
            .map(|definition| definition.constructor.clone());
        if let Some(constructor) = constructor {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            deferred.resolve(ctx, constructor);
            return Ok(promise);
        }
        if let Some(deferred) = self.hub.state.borrow().waiters.get(name) {
            return Ok(deferred.promise());
        }
        {
            let mut state = self.hub.state.borrow_mut();
            if state.waiters.len() >= MAX_REGISTRY_ENTRIES { return Err(OpError::new("QuotaExceededError", "custom element waiter limit")); }
            state.waiters.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element waiter limit"))?;
        }
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        self.hub
            .state
            .borrow_mut()
            .waiters
            .insert(name.into(), deferred);
        Ok(promise)
    }

    #[method(hint(js(ce_reactions)))]
    fn upgrade(&self, _ctx: &mut Ctx, root: &DomNode) -> OpResult<()> {
        self.hub.observe_mutations_for(&root.realm)?;
        self.hub.enqueue_upgrade_subtree(&root.realm, root.id, None)
    }

    #[method(hint(js(ce_reactions)))]
    fn initialize(&self, ctx:&mut Ctx,root:&DomNode)->OpResult<()> {
        let wrapper = root.realm.wrap(ctx, root.id);
        ctx.ensure_native_identity_owner::<DomNode>(&wrapper)?;
        let is_document=matches!(root.realm.session.borrow().document().kind(root.id),Ok(NodeKind::Document));
        let owner=root.realm.session.borrow().document().node_document(root.id).map_err(dom_error)?;
        if !self.hub.scoped && (is_document || !registry_hub(&root.realm,owner).is_some_and(|hub|Rc::ptr_eq(&hub.state,&self.hub.state))) {
            return Err(OpError::new("NotSupportedError","global registry cannot initialize this root"));
        }
        self.hub.observe_mutations_for(&root.realm)?;
        let shadow=root.realm.session.borrow().document().shadow_host(root.id).map_err(dom_error)?.is_some();
        if (is_document || shadow) && registry_hub(&root.realm,root.id).is_none() {set_registry(&root.realm,root.id,Some(&self.hub))?;}
        let ids={let session=root.realm.session.borrow();let document=session.document();let mut ids=Vec::new();let mut cursor=Some(root.id);
            while let Some(id)=cursor {if ids.len()>document.node_count(){return Err(OpError::new("QuotaExceededError","registry initialization traversal limit"));}
                if matches!(document.kind(id),Ok(NodeKind::Element{..})){
                    ids.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","registry initialization allocation"))?;
                    ids.push(id);
                }
                cursor=selector::next_descendant(document,root.id,id).map_err(dom_error)?;}ids};
        for id in ids {
            if registry_hub(&root.realm,id).is_none(){set_registry(&root.realm,id,Some(&self.hub))?;}
            let eligible={let session=root.realm.session.borrow();self.hub.is_upgrade_candidate(&root.realm,session.document(),id,None)?};
            if eligible {self.hub.state.borrow_mut().reactions.push_back(Reaction::Upgrade(RealmNode::new(&root.realm,id)));}
        }
        self.hub.schedule();Ok(())
    }
}

#[lumen_bind::methods]
impl ReactionDelivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        let hub = self.hub.upgrade().or_else(|| {
            // A retained scoped registry can enqueue work after its global
            // registry is gone. The agent queue owns the actual queued hub.
            agent_reactions(ctx).borrow().backup.front().map(|entry| entry.hub.clone())
        });
        hub.map_or(Ok(()), |hub| hub.flush(ctx))
    }
}

impl CustomElementHub {
    pub(crate) fn new(realm: Rc<DomRealm>, jobs: Rc<RefCell<Vec<Value>>>) -> Self {
        Self {
            // `observe_mutations_for` both records the realm and installs its
            // sink. Start empty so installation wires the initial realm too.
            attached_realms: Rc::new(RefCell::new(Vec::new())),
            scoped_documents: Rc::new(RefCell::new(Vec::new())),
            realm,
            state: Rc::new(RefCell::new(HubState::default())),
            jobs,
            delivery: Rc::new(RefCell::new(None)),
            scheduled: Rc::new(Cell::new(false)),
            scoped: false,
            registry: Rc::new(RefCell::new(None)),
        }
    }

    pub(crate) fn registry_value(&self, ctx: &mut Ctx) -> Value {
        if let Some(value) = self.registry.borrow().as_ref().and_then(WeakValue::upgrade) {
            return value;
        }
        let owner = self.state.borrow().reactions.owner.upgrade();
        let hub = owner.unwrap_or_else(|| {
            let hub = Rc::new(self.clone());
            self.state.borrow_mut().reactions.owner = Rc::downgrade(&hub);
            hub
        });
        let value = ctx.new_instance(DomCustomElementRegistry { hub });
        ctx.set_native_identity_owner::<DomCustomElementRegistry>(&value)
            .expect("CustomElementRegistry native brand");
        *self.registry.borrow_mut() = ctx.weak_value(&value);
        if !self.scoped {*self.realm.custom_element_registry.borrow_mut() = ctx.weak_value(&value);}
        value
    }

    /// Add the custom-element observer to the realm's existing mutation fanout.
    pub(crate) fn observe_mutations(&self) -> OpResult<()> {
        self.observe_mutations_for(&self.realm.clone())
    }

    fn observe_mutations_for(&self, attached: &Rc<DomRealm>) -> OpResult<()> {
        {
            let mut realms = self.attached_realms.borrow_mut();
            realms.retain(|realm| realm.strong_count() != 0);
            if realms
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .any(|realm| Rc::ptr_eq(&realm, attached))
            {
                return Ok(());
            }
            realms.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom-element realm allocation"))?;
        }
        let weak = Rc::downgrade(&self.state);
        let realm = Rc::downgrade(attached);
        let policy = {
            let mut stored = attached.custom_shadow_policy.borrow_mut();
            stored.get_or_insert_with(|| Rc::new(ShadowPolicy {
                primary: RefCell::new(if self.scoped {std::rc::Weak::new()} else {weak.clone()}), registries: RefCell::new(Vec::new()), associations: RefCell::new(HashMap::new()),
            })).clone()
        };
        {
            let mut registries = policy.registries.borrow_mut();
            registries.retain(|state| state.strong_count() != 0);
            if !registries.iter().any(|state| state.ptr_eq(&weak)) {
                if registries.len() >= 1024 { return Err(OpError::new("QuotaExceededError", "custom-element shadow registry limit")); }
                registries.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom-element shadow registry allocation"))?;
                registries.push(weak.clone());
            }
        }
        if !self.scoped && Rc::ptr_eq(attached, &self.realm) { *policy.primary.borrow_mut() = weak.clone(); }
        let shadow_policy = Rc::downgrade(&policy);
        let shadow_realm = realm.clone();
        attached.session.borrow_mut().document_mut().set_shadow_host_policy(Some(Rc::new(move |document, node| {
            let (Some(policy), Some(realm)) = (shadow_policy.upgrade(), shadow_realm.upgrade()) else { return true; };
            policy.allows(document, &realm, node)
        })));
        if !self.scoped && Rc::ptr_eq(attached,&self.realm) {
            let parser_realm=Rc::downgrade(attached);
            attached.session.borrow_mut().document_mut().set_parser_custom_element_predicate(Some(Rc::new(move |document,context,local,is_value| {
                let Some(realm)=parser_realm.upgrade() else {return false;};
                let Some(hub)=registry_hub_for_document(&realm,document,context) else {return false;};
                let found=definition_for_element(&hub.state.borrow(),local,is_value).is_some();found
            })));
            let birth_realm=Rc::downgrade(attached);
            attached.session.borrow_mut().document_mut().set_parser_element_birth_sink(Some(Rc::new(move |document,node,context,document_parser| {
                let Some(realm)=birth_realm.upgrade() else{return Ok(());};
                if document_parser {realm.stylesheet_links.record_parser_birth(document,node);}
                realm.object_resources.record_birth(document,node,document_parser);
                let registry=parser_birth_registry(&realm,document,context);
                remember_birth_registry(&realm,document,node,registry.as_ref())
            })));
        }
        let jobs = self.jobs.clone();
        let delivery = self.delivery.clone();
        let scheduled = self.scheduled.clone();
        self.attached_realms.borrow_mut().push(Rc::downgrade(attached));
        attached.add_mutation_sink(Rc::new(move |document, mutation| {
            let (Some(state), Some(attached)) = (weak.upgrade(), realm.upgrade()) else {
                return;
            };
            let key = RealmNode::new(&attached, mutation.target);
            let mut state = state.borrow_mut();
            let before = state.reactions.len();
            if let lumen_html::observe::ObservedKind::Attribute {
                name,
                old_value,
                namespace_uri,
            } = &mutation.kind
            {
                let name = if namespace_uri.is_some() {
                    name.rsplit(':').next().unwrap_or(name)
                } else {
                    name.as_str()
                };
                if let Some(definition_name) = state.upgraded.get(&key).filter(|_| state.callbacks_enabled(key)).cloned() {
                    if let Some(definition) = state.definitions.get(&definition_name) {
                        if definition
                            .observed_attributes
                            .contains(name)
                        {
                            let new_value = document
                                .get_attribute_ns(mutation.target, namespace_uri.as_deref(), name)
                                .ok()
                                .flatten();
                            if let Some(callback) = definition
                                .callbacks
                                .get("attributeChangedCallback")
                                .cloned()
                            {
                                state.reactions.push_back(Reaction::Attribute(
                                    key,
                                    callback,
                                    name.to_owned(),
                                    old_value.clone(),
                                    new_value,
                                    namespace_uri.clone(),
                                ));
                            }
                        }
                    }
                }
            }
            // Actual mutation roots determine reaction order. Ordinary removal
            // and insertion have distinct lifecycle steps; state-preserving
            // moves use their separate connectedMove path below.
            if let Some(moving) = attached.moving_node.get() {
                if matches!(&mutation.kind, lumen_html::observe::ObservedKind::ChildList { added: Some(node), .. } if *node == moving.root) &&
                    tree_connected(document, moving.root) {
                    let mut cursor = Some(moving.root);
                    let mut remaining = document.node_count();
                    while let Some(id) = cursor {
                        if remaining == 0 { break; }
                        remaining -= 1;
                        let key = RealmNode::new(&attached, id);
                        let callbacks = state.upgraded.get(&key).filter(|_| state.callbacks_enabled(key)).and_then(|name| state.definitions.get(name)).map(|definition| (
                            definition.callbacks.get("connectedMoveCallback").cloned(),
                            definition.callbacks.get("disconnectedCallback").cloned(),
                            definition.callbacks.get("connectedCallback").cloned(),
                        ));
                        if let Some((moved, disconnected, connected)) = callbacks {
                            if let Some(callback) = moved {
                                state.reactions.push_back(Reaction::Connected(key, callback));
                            } else if disconnected.is_some() || connected.is_some() {
                                state.reactions.push_back(Reaction::MoveFallback(key, disconnected, connected));
                            }
                        }
                        cursor = selector::next_shadow_including_descendant(document, moving.root, id).ok().flatten();
                    }
                }
            } else {
                for root in mutation.kind.removed_nodes() {
                    enqueue_connection_subtree(&mut state, document, &attached, root, false);
                }
                let (one, many): (Option<NodeId>, &[NodeId]) = match &mutation.kind {
                    lumen_html::observe::ObservedKind::ChildList {added, ..} => (*added, &[]),
                    lumen_html::observe::ObservedKind::ChildListMany {added, ..} => (None, added),
                    _ => (None, &[]),
                };
                for root in one.into_iter().chain(many.iter().copied()) {
                    enqueue_connection_subtree(&mut state, document, &attached, root, tree_connected(document, root));
                }
            }
            refresh_form_associations(&mut state,document,&attached);
            let queued = state.reactions.len() != before || state.reactions.allocation_failed;
            drop(state);
            let backup = queued && state_agent_schedule(&weak);
            if queued && backup && !scheduled.replace(true) {
                if let Some(callback) = delivery.borrow().as_ref() {
                    jobs.borrow_mut().push(callback.clone());
                }
            }
        }));
        Ok(())
    }

    fn owns_realm(&self, realm: &Rc<DomRealm>) -> bool {
        self.attached_realms
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|attached| Rc::ptr_eq(&attached, realm))
    }

    fn realm_for_node(&self, node: RealmNode) -> Option<Rc<DomRealm>> {
        self.attached_realms
            .borrow()
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .find(|realm| Rc::as_ptr(realm) as usize == node.realm)
    }

    fn define(
        &self,
        ctx: &mut Ctx,
        name: &str,
        constructor: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        if !ctx.value_is_constructor(&constructor) {
            return Err(OpError::new(
                "TypeError",
                "custom element constructor must be a constructor",
            ));
        }
        if !valid_name(name) {
            return Err(crate::error_reporting::dom_exception(ctx, "SyntaxError", "invalid custom element name"));
        }
        let extends = if let Some(options) = options.filter(|value| !matches!(value, Value::Undefined | Value::Null)) {
            if !matches!(options, Value::Obj(_)) { return Err(OpError::type_error("custom element definition options must be a dictionary")); }
            let value = ctx
                .member_get(&options, "extends")
                .map_err(OpError::thrown)?;
            if matches!(value, Value::Undefined) {
                None
            } else {
                Some(
                    ctx.coerce_string(&value)
                        .map_err(OpError::thrown)?
                        .to_string(),
                )
            }
        } else {
            None
        };
        if let Some(tag) = extends.as_deref() {
            if self.scoped {return Err(crate::error_reporting::dom_exception(ctx,"NotSupportedError","scoped registries cannot define customized built-in elements"));}
            if custom_name_tag(tag).is_none() {
                return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "customized built-in base interface is not implemented"));
            }
        }
        {
            let mut state = self.state.borrow_mut();
            if state.definitions.contains_key(name) || state.definitions.values()
                .any(|item| ctx.values_strict_equal(&item.constructor, &constructor)) {
                drop(state);
                return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "custom element name or constructor is already registered"));
            }
            if state.definition_running {
                drop(state);
                return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "custom element definition is already running"));
            }
            if state.definitions.len() >= MAX_REGISTRY_ENTRIES { return Err(OpError::new("QuotaExceededError", "custom element definition limit")); }
            state.definitions.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element definition limit"))?;
            state.definition_running = true;
        }
        let running = DefinitionRunning(self.state.clone());
        let prototype = ctx
            .member_get(&constructor, "prototype")
            .map_err(OpError::thrown)?;
        if !matches!(prototype, Value::Obj(_)) {
            return Err(OpError::new(
                "TypeError",
                "custom element prototype must be an object",
            ));
        }
        // A constructor may implement HTML construction with Reflect.construct
        // instead of a statically inherited prototype. HTMLConstructor checks
        // the actual called interface when construction occurs.
        let mut callbacks = HashMap::new();
        collect_definition_callbacks(ctx, &prototype, &[
            "connectedCallback",
            "disconnectedCallback",
            "connectedMoveCallback",
            "adoptedCallback",
            "attributeChangedCallback",
        ], &mut callbacks)?;
        let observed_attributes = if callbacks.contains_key("attributeChangedCallback") {
            match ctx.member_get(&constructor, "observedAttributes") {
                Ok(Value::Undefined) => HashSet::new(),
                Ok(value) => {
                    if !matches!(value, Value::Obj(_)) { return Err(OpError::type_error("observedAttributes must be an iterable object")); }
                    let mut attributes = HashSet::new();
                    ctx.convert_iterable(&value, 65_536, |ctx, item| {
                        let item = ctx.coerce_string(&item).map_err(OpError::thrown)?.to_string();
                        if !attributes.contains(&item) {
                            attributes.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "observed attribute limit"))?;
                            attributes.insert(item);
                        }
                        Ok(())
                    })?;
                    attributes
                }
                Err(error) => return Err(OpError::thrown(error)),
            }
        } else {
            HashSet::new()
        };
        let mut disabled_shadow = false;
        let mut disabled_internals = false;
        let disabled_features = ctx.member_get(&constructor, "disabledFeatures").map_err(OpError::thrown)?;
        if !matches!(disabled_features, Value::Undefined) {
            if !matches!(disabled_features, Value::Obj(_)) { return Err(OpError::type_error("disabledFeatures must be an iterable object")); }
            // Conversion observes the whole iterable and its exceptions, but
            // retains only the one compact policy bit needed by attachment.
            ctx.convert_iterable(&disabled_features, 65_536, |ctx, item| {
                let feature = ctx.coerce_string(&item).map_err(OpError::thrown)?;
                disabled_shadow |= feature.as_ref() == "shadow";
                disabled_internals |= feature.as_ref() == "internals";
                Ok(())
            })?;
        }
        let form_associated = ctx.member_get(&constructor, "formAssociated").map_err(OpError::thrown)?;
        if ctx.to_boolean(&form_associated) {
            collect_definition_callbacks(ctx, &prototype, &[
                "formAssociatedCallback", "formResetCallback", "formDisabledCallback", "formStateRestoreCallback",
            ], &mut callbacks)?;
        }
        drop(running);
        let definition = Definition {
            name: name.into(),
            extends,
            constructor: constructor.clone(),
            observed_attributes,
            callbacks,
            disabled_shadow,
            disabled_internals,
            form_associated:ctx.to_boolean(&form_associated),
        };
        let waiters = {
            let mut state = self.state.borrow_mut();
            if state.definitions.contains_key(name)
                || state
                    .definitions
                    .values()
                    .any(|item| ctx.values_strict_equal(&item.constructor, &constructor))
            {
                drop(state);
                return Err(crate::error_reporting::dom_exception(ctx, "NotSupportedError", "custom element name or constructor is already registered"));
            }
            state.definitions.insert(name.into(), definition);
            state.waiters.remove(name)
        };
let documents:Vec<_>=if self.scoped {
            self.scoped_documents.borrow().iter().filter_map(|(realm,root)|realm.upgrade().map(|realm|(realm,*root))).collect()
        } else {vec![(self.realm.clone(),self.realm.session.borrow().document().root())]};
        for (realm,root) in documents {self.enqueue_upgrade_subtree(&realm,root,Some(name))?;}
        if let Some(waiter) = waiters {
            waiter.resolve(ctx, constructor.clone());
        }
        Ok(())
    }

    fn upgrade_subtree(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId) -> OpResult<()> {
        self.upgrade_subtree_with_reporting(ctx, realm, root, true)
    }

    fn upgrade_candidates(&self, realm: &Rc<DomRealm>, root: NodeId, definition_name: Option<&str>) -> OpResult<Vec<NodeId>> {
        if !self.owns_realm(realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "root is not owned by this custom element registry",
            ));
        }
        if self.state.borrow().definitions.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = {
            let session = realm.session.borrow();
            let document = session.document();
            let mut ids = Vec::new();
            let mut cursor = Some(root);
            let mut visited = 0usize;
            while let Some(id) = cursor {
                visited += 1;
                if visited > document.node_count() {
                    return Err(OpError::new("QuotaExceededError", "custom element traversal limit"));
                }
                if self.is_upgrade_candidate(realm,document,id,definition_name)? {
                        ids.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element candidate allocation"))?;
                        ids.push(id);
                }
                cursor = selector::next_shadow_including_descendant(document, root, id).map_err(dom_error)?;
            }
            ids
        };
        Ok(candidates)
    }

    fn is_upgrade_candidate(&self,realm:&Rc<DomRealm>,document:&lumen_html::Document,id:NodeId,definition_name:Option<&str>)->OpResult<bool> {
        if !matches!(document.kind(id),Ok(NodeKind::Element{namespace:Namespace::Html,..})){return Ok(false);}
        if !registry_hub_for_document(realm,document,id).is_some_and(|hub|Rc::ptr_eq(&hub.state,&self.state)){return Ok(false);}
        let (_,local)=document.element_name_parts(id).map_err(dom_error)?;
        let is_value=document.custom_element_is_value(id).map_err(dom_error)?;
        let key=RealmNode::new(realm,id);
        let state=self.state.borrow();
        Ok(!state.upgraded.contains_key(&key) && !state.failed.contains(&key)
            && !state.pending_upgrade.iter().any(|pending|pending.node==id && Rc::ptr_eq(&pending.realm,realm))
            && definition_for_element(&state,local,is_value).is_some_and(|definition|definition_name.is_none_or(|name|definition.name==name)))
    }

fn upgrade_subtree_with_reporting(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId, report_errors: bool) -> OpResult<()> {
    for id in self.upgrade_candidates(realm, root, None)? { self.upgrade_one(ctx, realm, id, report_errors)?; }
    Ok(())
}

fn enqueue_upgrade_subtree(&self, realm: &Rc<DomRealm>, root: NodeId, definition_name: Option<&str>) -> OpResult<()> {
    for id in self.upgrade_candidates(realm, root, definition_name)? {
        self.state.borrow_mut().reactions.push_back(Reaction::Upgrade(RealmNode::new(realm, id)));
    }
    self.schedule();
    Ok(())
}

fn upgrade_one(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, id: NodeId, report_errors: bool) -> OpResult<()> {
    self.construct_one(ctx,realm,id,report_errors,false)
}

fn construct_one(&self, ctx:&mut Ctx,realm:&Rc<DomRealm>,id:NodeId,report_errors:bool,fresh:bool)->OpResult<()> {

            // An earlier constructor may remove or adopt another candidate.
            if realm.session.borrow().document().kind(id).is_err() { return Ok(()); }
            if !matches!(realm.session.borrow().document().kind(id),Ok(NodeKind::Element{namespace:Namespace::Html,..})){return Ok(());}
            let key = RealmNode::new(&realm, id);
            let original_document=realm.session.borrow().document().node_document(id).map_err(dom_error)?;
            let constructor = {
                let state = self.state.borrow();
                if state.upgraded.contains_key(&key) || state.failed.contains(&key) ||
                    state.pending_upgrade.iter().any(|pending| pending.node == id && Rc::ptr_eq(&pending.realm, realm)) {
                    return Ok(());
                }
                let session = realm.session.borrow();
                let document = session.document();
                let (_, local) = document.element_name_parts(id).map_err(dom_error)?;
                let is_value = document.custom_element_is_value(id).map_err(dom_error)?;
                definition_for_element(&state, local, is_value)
                    .map(|definition| (definition.name.clone(), definition.constructor.clone()))
            };
            if let Some((custom_name, constructor)) = constructor {
                let expected = realm.wrap(ctx, id);
                // Upgrade snapshots initial attributes and connectedness before
                // invoking author code. The provisional definition also makes
                // mutations after super() enqueue their own later reactions.
                let disabled_shadow = self.state.borrow().definitions.get(&custom_name)
                    .is_some_and(|definition| definition.disabled_shadow);
                if disabled_shadow && realm.session.borrow().document().shadow_root(id).map_err(dom_error)?.is_some() {
                    self.state.borrow_mut().failed.insert(key);
                    let error = crate::error_reporting::dom_exception(ctx,
                        "NotSupportedError", "custom element definition disables an existing shadow root");
                    handle_upgrade_exception_for_constructor(ctx, &constructor, error, report_errors)?;
                    return Ok(());
                }
                {
                    let mut state = self.state.borrow_mut();
                    if state.pending_upgrade.len() >= MAX_REGISTRY_ENTRIES {
                        return Err(OpError::new("QuotaExceededError", "custom element construction stack limit"));
                    }
                    state.pending_upgrade.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element construction stack allocation"))?;
                    state.failed.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element failed state allocation"))?;
                }
                let active_constructor = active_constructor_scope(self, &constructor)?;
                self.mark_upgraded(realm, id, custom_name.clone())?;
                self.state
                    .borrow_mut()
                    .pending_upgrade
                    .push(PendingUpgrade {
                        realm: realm.clone(),
                        node: id,
                        definition: custom_name.clone(),
                        consumed_by_super: false,
                    });
                let pending_construction = PendingConstruction(self.state.clone());
                let result = ctx.construct_value(constructor.clone(), &[]);
                drop(active_constructor);
                let consumed = self
                    .state
                    .borrow()
                    .pending_upgrade
                    .last()
                    .map(|pending| pending.consumed_by_super)
                    .unwrap_or(false);
                drop(pending_construction);
                let constructed = match result {
                    Ok(value) => value,
                    Err(error) => {
                        let mut state = self.state.borrow_mut();
                        state.upgraded.remove(&key);
                        state.connected.remove(&key);
                        state.reactions.retain(|reaction| reaction.node() != key);
                        state.failed.insert(key);
                        drop(state);
                        handle_upgrade_exception_for_constructor(ctx, &constructor, OpError::thrown(error), report_errors)?;
                        return Ok(());
                    }
                };
                if !ctx.values_strict_equal(&constructed, &expected) {
                    let mut state = self.state.borrow_mut();
                    state.upgraded.remove(&key);
                    state.connected.remove(&key);
                    state.reactions.retain(|reaction| reaction.node() != key);
                    state.failed.insert(key);
                    drop(state);
                    handle_upgrade_exception_for_constructor(ctx, &constructor, OpError::new(
                        "TypeError",
                        "custom element constructor did not return its upgraded element",
                    ), report_errors)?;
                    return Ok(());
                }
                if !consumed {
                    let mut state = self.state.borrow_mut();
                    state.upgraded.remove(&key);
                    state.connected.remove(&key);
                    state.reactions.retain(|reaction| reaction.node() != key);
                    state.failed.insert(key);
                    drop(state);
                    let error = crate::error_reporting::dom_exception(ctx, "NotSupportedError",
                        "custom element constructor did not call super()");
                    handle_upgrade_exception_for_constructor(ctx, &constructor, error, report_errors)?;
                    return Ok(());
                }
                let autonomous=self.state.borrow().definitions.get(&custom_name).is_some_and(|definition|definition.extends.is_none());
                if fresh && autonomous {
                    let valid={let session=realm.session.borrow();let document=session.document();
                        matches!(document.kind(id),Ok(NodeKind::Element{attributes,..}) if attributes.is_empty())
                        && document.first_child(id).ok().flatten().is_none()
                        && document.parent(id).ok().flatten().is_none()
                        && document.node_document(id).ok()==Some(original_document)};
                    if !valid {
                        let mut state=self.state.borrow_mut();state.upgraded.remove(&key);state.connected.remove(&key);state.reactions.retain(|reaction|reaction.node()!=key);state.failed.insert(key);drop(state);
                        let error=crate::error_reporting::dom_exception(ctx,"NotSupportedError","fresh custom element constructor changed attributes, children, or parent");
                        handle_upgrade_exception_for_constructor(ctx,&constructor,error,report_errors)?;
                        return Ok(());
                    }
                }
                self.complete_form_element(realm,id)?;
            }
        Ok(())
    }

    fn complete_form_element(&self,realm:&Rc<DomRealm>,id:NodeId)->OpResult<()> {
        let key=RealmNode::new(realm,id);
        let associated={let state=self.state.borrow();state.upgraded.get(&key).and_then(|name|state.definitions.get(name)).is_some_and(|definition|definition.form_associated && definition.extends.is_none())};
        if associated && !realm.forms.borrow().is_custom_form_control(id) {
            realm.forms.borrow_mut().complete_custom_form_control(id)?;
            let session=realm.session.borrow();let document=session.document();
            let mut state=self.state.borrow_mut();state.form_states.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","custom form association allocation"))?;
            state.form_states.insert(key,(None,false));refresh_form_associations(&mut state,document,realm);
            drop(state);self.schedule();
        }Ok(())
    }

    fn invoke_reaction(&self, ctx: &mut Ctx, reaction: Reaction) {
        let mut args = [Value::Undefined, Value::Undefined, Value::Undefined, Value::Undefined];
        let (id, callback, argument_count) = match reaction {
            Reaction::Upgrade(id) => {
                if let Some(realm) = self.realm_for_node(id) {
                    if let Err(error) = self.upgrade_one(ctx, &realm, id.node, true) {
                        let exception = error.to_value(ctx);
                        crate::error_reporting::report_exception(ctx, exception);
                    }
                }
                return;
            }
            Reaction::Connected(id, callback) | Reaction::Disconnected(id, callback) => (id, callback, 0),
            Reaction::Attribute(id, callback, name, old, new, namespace) => {
                args = [Value::from_string(name), old.map(Value::from_string).unwrap_or(Value::Null),
                    new.map(Value::from_string).unwrap_or(Value::Null), namespace.map(Value::from_string).unwrap_or(Value::Null)];
                (id, callback, 4)
            }
            Reaction::FormAssociated(id, callback, owner, _retention) => {
                let realm=self.realm_for_node(id).unwrap_or_else(||self.realm.clone());
                args[0]=owner.map(|owner|realm.wrap(ctx,owner)).unwrap_or(Value::Null);
                (id,callback,1)
            }
            Reaction::FormDisabled(id,callback,disabled)=>{args[0]=Value::Bool(disabled);(id,callback,1)}
            Reaction::FormReset(id,callback)=>(id,callback,0),
            Reaction::FormRestore(id,callback,value)=>{args[0]=value;args[1]=Value::from_string("restore".into());(id,callback,2)},
            Reaction::Adopted(id, callback, old, new) => {
                args[0] = old; args[1] = new;
                (id, callback, 2)
            }
            Reaction::AdoptedPending(id,callback,old,new)=>{
                args[0]=old.value(ctx);args[1]=new.value(ctx);
                (id,callback,2)
            }
            Reaction::MoveFallback(id, disconnected, connected) => {
                let target = self.realm_for_node(id).unwrap_or_else(|| self.realm.clone()).wrap(ctx, id.node);
                if let Some(callback) = disconnected {
                    if !invoke_lifecycle_callback(ctx, callback, target.clone(), &[]) { return; }
                }
                if let Some(callback) = connected {
                    invoke_lifecycle_callback(ctx, callback, target, &[]);
                }
                return;
            }
        };
        if callback.is_callable() {
            let target = self.realm_for_node(id).unwrap_or_else(|| self.realm.clone()).wrap(ctx, id.node);
            invoke_lifecycle_callback(ctx, callback, target, &args[..argument_count]);
        }
    }

    fn refresh_pending_form_associations(&self) {
        if !self.state.borrow().form_refresh_pending{return;}
        self.state.borrow_mut().form_refresh_pending=false;
        for realm in self.attached_realms.borrow().iter().filter_map(std::rc::Weak::upgrade){
            if let Ok(session)=realm.session.try_borrow(){refresh_form_associations(&mut self.state.borrow_mut(),session.document(),&realm);}
            else{self.state.borrow_mut().form_refresh_pending=true;}
        }
    }

    pub(crate) fn deliver_reactions(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.refresh_pending_form_associations();
        let agent = self.state.borrow().reactions.agent.as_ref().and_then(std::rc::Weak::upgrade);
        if let Some(agent) = agent {
            // Keep the processing flag set for the entire invocation, including
            // callbacks that enqueue more backup work or enter nested scopes.
            {
                let mut state = agent.borrow_mut();
                if state.processing_backup { return Ok(()); }
                state.processing_backup = true;
                state.backup_scheduled = false;
            }
            loop {
                let queue = std::mem::take(&mut agent.borrow_mut().backup);
                if queue.is_empty() { break; }
                invoke_element_queue(ctx, &agent, queue);
            }
            agent.borrow_mut().processing_backup = false;
        } else {
            while let Some(reaction) = { self.state.borrow_mut().reactions.pop_front() } {
                self.invoke_reaction(ctx, reaction);
            }
        }
        let failed = std::mem::take(&mut self.state.borrow_mut().reactions.allocation_failed);
        if failed { return Err(OpError::new("QuotaExceededError", "custom element reaction queue limit")); }
        Ok(())
    }

    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.scheduled.set(false);
        self.deliver_reactions(ctx)
    }

    pub(crate) fn constructor_for_new_target(
        &self,
        ctx: &Ctx,
        new_target: &Value,
    ) -> Option<(String, Option<String>)> {
        self.state
            .borrow()
            .definitions
            .values()
            .find(|definition| ctx.values_strict_equal(&definition.constructor, new_target))
            .map(|definition| (definition.name.clone(), definition.extends.clone()))
    }

    fn consume_pending_upgrade(&self, definition: &str) -> OpResult<Option<(Rc<DomRealm>, NodeId)>> {
        let mut state = self.state.borrow_mut();
        let Some(pending) = state.pending_upgrade.iter_mut().rev()
            .find(|pending| pending.definition == definition) else { return Ok(None); };
        if pending.consumed_by_super {
            return Err(OpError::type_error("custom element construction stack is already constructed"));
        }
        pending.consumed_by_super = true;
        Ok(Some((pending.realm.clone(), pending.node)))
    }

    pub(crate) fn mark_upgraded(&self, realm: &Rc<DomRealm>, id: NodeId, name: String) -> OpResult<()> {
        let key = RealmNode::new(realm, id);
        let (connected, attributes, attribute_callback) = {
            let session = realm.session.borrow();
            let document = session.document();
            let state = self.state.borrow();
            let definition = state.definitions.get(&name);
            let attribute_callback = definition.and_then(|definition| definition.callbacks.get("attributeChangedCallback")).cloned();
            let mut captured = Vec::new();
            if attribute_callback.is_some() {
                if let Ok(NodeKind::Element { attributes, .. }) = document.kind(id) {
                    for (index, (name, value)) in attributes.iter().enumerate() {
                        let namespace = document.attribute_namespace_uri_at(id, index);
                        let local = if namespace.is_some() {
                            name.as_str().rsplit(':').next().unwrap_or(name.as_str())
                        } else {
                            name.as_str()
                        };
                        if definition.is_some_and(|definition| definition.observed_attributes.contains(local)) {
                            captured.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element attribute snapshot allocation"))?;
                            captured.push((local.to_owned(), value.clone(), namespace.map(str::to_owned)));
                        }
                    }
                }
            }
            (tree_connected(document, id), captured, attribute_callback)
        };
        let mut state = self.state.borrow_mut();
        state.upgraded.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element state allocation"))?;
        state.connected.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "custom element connection state allocation"))?;
        state.upgraded.insert(key, name.clone());
        state.connected.insert(key, connected);
        if let Some(callback) = attribute_callback {
            for (attribute, value, namespace) in attributes {
                    state.reactions.push_back(Reaction::Attribute(
                        key,
                        callback.clone(),
                        attribute,
                        None,
                        Some(value),
                        namespace,
                    ));
            }
        }
        if connected {
            if let Some(callback) = state
                .definitions
                .get(&name)
                .and_then(|definition| definition.callbacks.get("connectedCallback"))
                .cloned() {
                state.reactions.push_back(Reaction::Connected(key, callback));
            }
        }
        drop(state);
        self.schedule();
        Ok(())
    }

    fn schedule(&self) {
        if let Some(agent) = self.state.borrow().reactions.agent.as_ref().and_then(std::rc::Weak::upgrade) {
            if !agent.borrow_mut().schedule_backup() { return; }
        }
        if !self.scheduled.replace(true) {
            if let Some(callback) = self.delivery.borrow().as_ref() {
                self.jobs.borrow_mut().push(callback.clone());
            }
        }
    }
}

/// Resolve explicit author content attributes before a custom element's ARIA
/// defaults. Embedders use this for accessibility policy, never DOM reflection.
pub fn effective_aria_attribute(ctx:&mut Ctx,target:&Value,attribute:&str)->OpResult<Value> {
    let (realm,node)=ctx.with_instance::<DomElement,_>(target,|element|element.base.realm.resolve_adopted_node(element.base.id))?;
    let authored=realm.session.borrow().document().get_attribute_ns_ref(node,None,attribute).map_err(dom_error)?.map(str::to_owned);
    if let Some(authored)=authored{return Ok(Value::from_string(authored));}
    let Some(internals)=ctx.native_private_value_slot(target,ATTACHED_INTERNALS_SLOT) else{return Ok(Value::Null);};
    ctx.with_instance::<DomElementInternals,_>(&internals,|internals|internals.aria_value(attribute))
}

/// Put a hub in interpreter state and attach its mutation observer. The caller installs the
/// returned singleton as `globalThis.customElements`.
pub(crate) fn install(ctx: &mut Ctx, realm: Rc<DomRealm>) -> OpResult<()> {
let agent = agent_reactions(ctx);
// Web IDL setlike keys and values are the same function object.
let constructor=ctx.class_constructor::<DomCustomStateSet>();
let prototype=ctx.member_get(&constructor,"prototype").map_err(OpError::thrown)?;
let values=ctx.member_get(&prototype,"values").map_err(OpError::thrown)?;
ctx.member_set(&prototype,"keys",values).map_err(OpError::thrown)?;
    ctx.op_state().put(lumen::embed::NativeOperationHooks {
        begin: begin_reaction_scope, end: end_reaction_scope, abort: abort_reaction_scope,
    });
    let hub = CustomElementHub::new(realm, ctx.deferred_microtasks());
    hub.observe_mutations()?;
    let shared_hub = Rc::new(hub.clone());
    crate::realm_services::RealmServices::replace_current(ctx, Rc::downgrade(&shared_hub));
    {
        let mut state = hub.state.borrow_mut();
        state.reactions.agent = Some(Rc::downgrade(&agent));
        state.reactions.owner = Rc::downgrade(&shared_hub);
    }
    *hub.realm.custom_element_hub.borrow_mut() = Rc::downgrade(&shared_hub);
    let root=hub.realm.session.borrow().document().root();
    associate_parsed_subtree(&hub.realm,root,root)?;
    bind_delivery(ctx, &shared_hub)?;
    let registry = hub.registry_value(ctx);
    let constructor = ctx.class_constructor::<DomCustomElementRegistry>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "CustomElementRegistry", constructor)
        .and_then(|_| ctx.member_set(&global, "customElements", registry))
        .map_err(|_| OpError::new("Error", "CustomElementRegistry installation failed"))
}

fn bind_delivery(ctx: &mut Ctx, hub: &Rc<CustomElementHub>) -> OpResult<()> {
    ctx.class_constructor::<ReactionDelivery>();
    let delivery = ctx.new_instance(ReactionDelivery { hub: Rc::downgrade(hub) });
    let flush = ctx
        .get_member(&delivery, "flush")
        .map_err(|_| OpError::new("Error", "custom element reaction delivery is unavailable"))?;
    let bind = ctx.get_member(&flush, "bind").map_err(|_| {
        OpError::new(
            "Error",
            "custom element reaction delivery bind is unavailable",
        )
    })?;
    let bound = JsFunction::from_value(bind)
        .ok_or_else(|| {
            OpError::new(
                "TypeError",
                "custom element reaction delivery bind is invalid",
            )
        })?
        .call(ctx, flush, &[delivery])?;
    *hub.delivery.borrow_mut() = Some(bound);
    Ok(())
}

fn refresh_form_associations(state:&mut HubState,document:&lumen_html::Document,realm:&Rc<DomRealm>) {
    if realm.forms.try_borrow().is_err(){state.form_refresh_pending=true;return;}
    // Only completed form-associated elements allocate association snapshots.
    let keys:Vec<_>=state.form_states.keys().filter(|key|key.realm==Rc::as_ptr(realm) as usize).copied().collect();
    for key in keys {
        if document.kind(key.node).is_err(){state.form_states.remove(&key);continue;}
        let owner=lumen_html::forms::form_owner(document,key.node);let disabled=lumen_html::forms::is_disabled(document,key.node);
        let old=state.form_states.insert(key,(owner,disabled)).unwrap_or((None,false));
        let callbacks=state.upgraded.get(&key).and_then(|name|state.definitions.get(name)).map(|definition|(definition.callbacks.get("formAssociatedCallback").cloned(),definition.callbacks.get("formDisabledCallback").cloned()));
        if let Some(callbacks)=callbacks {
            if old.0!=owner {if let Some(callback)=&callbacks.0 {
                let retention=owner.map(|owner|Rc::new(NodeRetention::new(realm,owner)));
                state.reactions.push_back(Reaction::FormAssociated(key,callback.clone(),owner,retention));
            }}
            if old.1!=disabled {if let Some(callback)=&callbacks.1{state.reactions.push_back(Reaction::FormDisabled(key,callback.clone(),disabled));}}
        }
    }
}

/// Persisted FACE state uses document-order control locators and shared blob
/// snapshots; it contains no NodeId, realm, JavaScript value, or registry root.
pub(crate) struct StoredCustomFormControl {
    ordinal:usize,
    local_name:String,
    name:String,
    value:lumen_host::blob::StoredFormValue,
}
impl StoredCustomFormControl {
    pub(crate) fn retained_bytes(&self)->usize {std::mem::size_of::<Self>().saturating_add(self.local_name.len()).saturating_add(self.name.len()).saturating_add(self.value.retained_bytes())}
}
pub(crate) fn snapshot_custom_form_state(ctx:&mut Ctx,realm:&Rc<DomRealm>)->OpResult<Vec<StoredCustomFormControl>> {
    let pending={
        let session=realm.session.borrow();let document=session.document();let forms=realm.forms.borrow();
        let root=document.root();let mut cursor=Some(root);let mut ordinal=0;let mut pending=Vec::new();
        while let Some(node)=cursor {
            if document.is_form_associated_custom_element(node) {
                if let Some(value)=forms.custom_elements.get(&node).and_then(|state|state.borrow().restore.clone()) {
                    let (_,name)=document.element_name_parts(node).map_err(dom_error)?;
                    pending.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","custom form state allocation"))?;
                    pending.push((ordinal,name.to_owned(),document.get_attribute_ns_ref(node,None,"name").map_err(dom_error)?.unwrap_or("").to_owned(),value));
                }
                ordinal+=1;
            }
            cursor=selector::next_shadow_including_descendant(document,root,node).map_err(dom_error)?;
        }pending
    };
    let mut result=Vec::new();let mut bytes=0usize;
    for (ordinal,local_name,name,value) in pending {
        let value=lumen_host::blob::snapshot_form_value(ctx,&value)?;
        let stored=StoredCustomFormControl{ordinal,local_name,name,value};bytes=bytes.saturating_add(stored.retained_bytes());
        if bytes>1024*1024{return Err(OpError::new("QuotaExceededError","custom form state storage limit"));}
        result.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","custom form state allocation"))?;result.push(stored);
    }Ok(result)
}
pub(crate) fn restore_custom_form_state(ctx:&mut Ctx,realm:&Rc<DomRealm>,stored:&[StoredCustomFormControl])->OpResult<()> {
    let targets={
        let session=realm.session.borrow();let document=session.document();let root=document.root();
        let mut cursor=Some(root);let mut ordinal=0;let mut index=0;let mut targets=Vec::new();
        while let Some(node)=cursor {
            if document.is_form_associated_custom_element(node) {
                while index<stored.len() && stored[index].ordinal<ordinal{index+=1;}
                if let Some(state)=stored.get(index).filter(|state|state.ordinal==ordinal) {
                    let (_,name)=document.element_name_parts(node).map_err(dom_error)?;
                    if name==state.local_name && document.get_attribute_ns_ref(node,None,"name").map_err(dom_error)?.unwrap_or("")==state.name {
                        targets.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","custom form restore allocation"))?;targets.push((node,index));
                    }
                }ordinal+=1;
            }
            cursor=selector::next_shadow_including_descendant(document,root,node).map_err(dom_error)?;
        }targets
    };
    begin_reaction_scope(ctx).map_err(OpError::thrown)?;
    let result=(|| {
        for (node,index) in targets {
            let Some(hub)=registry_hub(realm,node) else{continue;};let key=RealmNode::new(realm,node);
            let callback={let state=hub.state.borrow();state.upgraded.get(&key).and_then(|name|state.definitions.get(name)).and_then(|definition|definition.callbacks.get("formStateRestoreCallback")).cloned()};
            let Some(callback)=callback else{continue;};
            let value=lumen_host::blob::restore_form_value(ctx,&stored[index].value)?;
            hub.state.borrow_mut().reactions.push_back(Reaction::FormRestore(key,callback,value));hub.schedule();
        }Ok(())
    })();
    end_reaction_scope(ctx);result
}

pub(crate) fn enqueue_form_reset(realm:&Rc<DomRealm>,form:NodeId)->OpResult<()> {
    let nodes:Vec<_>={let state=realm.forms.borrow();state.custom_elements.iter().filter_map(|(node,state)|state.borrow().completed.then_some(*node)).collect()};
    for node in nodes {
        if realm.session.borrow().document().kind(node).is_err(){continue;}
        if lumen_html::forms::form_owner(realm.session.borrow().document(),node)!=Some(form){continue;}
        if let Some(hub)=registry_hub(realm,node){
            let key=RealmNode::new(realm,node);let mut state=hub.state.borrow_mut();
            let callback=state.upgraded.get(&key).and_then(|name|state.definitions.get(name)).and_then(|definition|definition.callbacks.get("formResetCallback")).cloned();
            if let Some(callback)=callback{state.reactions.push_back(Reaction::FormReset(key,callback));}drop(state);hub.schedule();
        }
    }Ok(())
}

pub(crate) fn hub_from_ctx(ctx: &mut Ctx) -> Option<CustomElementHub> {
    crate::realm_services::RealmServices::<std::rc::Weak<CustomElementHub>>::current(ctx)
        .and_then(|hub| hub.upgrade()).map(|hub| (*hub).clone())
}

/// Sparse exceptions to the document's ordinary global registry. Explicit null
/// is distinct from the absent/default association; hubs stay weak to avoid an
/// Rc cycle through their owning document.
fn explicit_registry(realm:&DomRealm,id:NodeId)->Option<Option<Rc<CustomElementHub>>> {
    realm.custom_shadow_policy.borrow().as_ref().and_then(|policy|
        policy.associations.borrow().get(&id).map(|hub|hub.as_ref().and_then(std::rc::Weak::upgrade)))
}

pub(crate) fn registry_hub(realm:&Rc<DomRealm>,id:NodeId)->Option<Rc<CustomElementHub>> {
    let session=realm.session.borrow();registry_hub_for_document(realm,session.document(),id)
}

fn registry_hub_for_document(realm:&Rc<DomRealm>,document:&lumen_html::Document,id:NodeId)->Option<Rc<CustomElementHub>> {
    if let Some(registry)=explicit_registry(realm,id) {return registry;}
    if document.shadow_options(id).ok().flatten().is_some_and(|options|options.keep_custom_element_registry_null) {return None;}
    if !matches!(document.kind(id),Ok(NodeKind::Element{..}|NodeKind::Document)) && document.shadow_host(id).ok().flatten().is_none(){return None;}
    let owner=document.node_document(id).ok()?;
    if document.is_template_owner_document(owner) {return None;}
    if let Some(registry)=explicit_registry(realm,owner) {return registry;}
    realm.custom_element_hub.borrow().upgrade()
}

fn parser_birth_registry(realm:&Rc<DomRealm>,document:&lumen_html::Document,context:NodeId)->Option<Rc<CustomElementHub>> {
    // Template fragment parsing happens in the associated inert document,
    // rather than inheriting the template element's own scoped association.
    if document.template_content(context).ok().flatten().is_some(){return None;}
    registry_hub_for_document(realm,document,context)
}

fn remember_birth_registry(realm:&Rc<DomRealm>,document:&lumen_html::Document,id:NodeId,hub:Option<&Rc<CustomElementHub>>)->Result<(),lumen_html::Error> {
    let owner=document.node_document(id)?;
    let ordinary=registry_hub_for_document(realm,document,owner);
    if hub.is_some_and(|hub|!hub.scoped && ordinary.as_ref().is_some_and(|ordinary|Rc::ptr_eq(&hub.state,&ordinary.state))) {
        return Ok(());
    }
    remember_registry(realm,document,id,hub)
}

fn remember_registry(realm:&Rc<DomRealm>,document:&lumen_html::Document,id:NodeId,hub:Option<&Rc<CustomElementHub>>)->Result<(),lumen_html::Error> {
    if let Some(hub)=hub.filter(|hub|hub.scoped) {
        let owner=document.node_document(id)?;
        let mut documents=hub.scoped_documents.borrow_mut();
        documents.retain(|(realm,_)|realm.strong_count()!=0);
        if !documents.iter().any(|(stored,node)|*node==owner && stored.ptr_eq(&Rc::downgrade(realm))) {
            if documents.len()>=MAX_REGISTRY_ENTRIES{return Err(lumen_html::Error::LimitExceeded);}
            documents.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
            documents.push((Rc::downgrade(realm),owner));
        }
    }
    let mut stored=realm.custom_shadow_policy.borrow_mut();
    let policy=stored.get_or_insert_with(||Rc::new(ShadowPolicy {primary:RefCell::new(std::rc::Weak::new()),registries:RefCell::new(Vec::new()),associations:RefCell::new(HashMap::new())}));
    let mut map=policy.associations.borrow_mut();
    if !map.contains_key(&id) {
        if map.len()>=MAX_REGISTRY_ENTRIES {map.retain(|node,_|document.kind(*node).is_ok());}
        if map.len()>=MAX_REGISTRY_ENTRIES{return Err(lumen_html::Error::LimitExceeded);}
        map.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
    }
    map.insert(id,hub.map(Rc::downgrade));
    Ok(())
}

fn set_registry(realm:&Rc<DomRealm>,id:NodeId,hub:Option<&Rc<CustomElementHub>>)->OpResult<()> {
    if let Some(hub)=hub {hub.observe_mutations_for(realm)?;}
    remember_registry(realm,realm.session.borrow().document(),id,hub).map_err(dom_error)
}

pub(crate) fn registry_value_for_node(ctx:&mut Ctx,realm:&Rc<DomRealm>,id:NodeId)->Value {
    registry_hub(realm,id).map_or(Value::Null,|hub|hub.registry_value(ctx))
}

pub(crate) fn trace_registry_for_node(realm:&DomRealm,id:NodeId,visit:&mut dyn FnMut(&Value)) {
    let hub=explicit_registry(realm,id).unwrap_or_else(||realm.custom_element_hub.borrow().upgrade());
    if let Some(hub)=hub {
        if let Some(value)=hub.registry.borrow().as_ref().and_then(WeakValue::upgrade) {visit(&value);}
    }
}

pub(crate) fn is_fresh_custom_element_failure(realm:&Rc<DomRealm>,id:NodeId)->bool {
    let key=RealmNode::new(realm,id);
    realm.custom_shadow_policy.borrow().as_ref().is_some_and(|policy|
        policy.registries.borrow().iter().filter_map(std::rc::Weak::upgrade).any(|state|state.borrow().fresh_fallbacks.contains(&key)))
}

pub(crate) fn registry_option(ctx:&mut Ctx,realm:&Rc<DomRealm>,document:NodeId,options:Option<&Value>)->OpResult<Option<Option<Rc<CustomElementHub>>>> {
    let Some(options)=options.filter(|value|matches!(value,Value::Obj(_))) else {return Ok(None);};
    let registry=ctx.member_get(options,"customElementRegistry").map_err(OpError::thrown)?;
    if matches!(registry,Value::Undefined) {return Ok(None);}
    if matches!(registry,Value::Null) {return Ok(Some(None));}
    validate_registry_for_document(ctx,realm,document,&registry)?;
    let hub=ctx.with_instance::<DomCustomElementRegistry,_>(&registry,|registry|registry.hub.clone())?;
    *hub.registry.borrow_mut()=ctx.weak_value(&registry);
    Ok(Some(Some(hub)))
}

pub(crate) fn associate_created(realm:&Rc<DomRealm>,id:NodeId,registry:Option<Option<Rc<CustomElementHub>>>)->OpResult<()> {
    if let Some(registry)=registry {set_registry(realm,id,registry.as_ref())?;}
    else if let Some(hub)=registry_hub(realm,id).filter(|hub|hub.scoped) {set_registry(realm,id,Some(&hub))?;}
    else if registry_hub(realm,id).is_none(){set_registry(realm,id,None)?;}
    Ok(())
}

pub(crate) fn construct_parser_created_element(ctx:&mut Ctx,realm:&Rc<DomRealm>,id:NodeId,append_attributes:impl FnOnce(&mut Ctx,NodeId)->OpResult<()>)->OpResult<NodeId> {
    let (kind,is_value,prefix)={let session=realm.session.borrow();let document=session.document();
        (document.kind(id).map_err(dom_error)?.clone(),document.custom_element_is_value(id).map_err(dom_error)?.map(str::to_owned),
        document.element_name_parts(id).map_err(dom_error)?.0.map(str::to_owned))};
    begin_reaction_scope(ctx).map_err(OpError::thrown)?;
    let mut constructed_value = None;
    let result=(|| {
        let mut chosen=id;
        if let Some(hub)=registry_hub(realm,id) {
            let autonomous = if matches!(&kind,NodeKind::Element{namespace:Namespace::Html,..}) {
                let state = hub.state.borrow();
                let session = realm.session.borrow();
                let document = session.document();
                let (_, local) = document.element_name_parts(id).map_err(dom_error)?;
                definition_for_element(&state, local, is_value.as_deref())
                    .filter(|definition| definition.extends.is_none())
                    .map(|definition| (definition.constructor.clone(), local.to_owned(), document.node_document(id)))
            } else { None };
            let fresh_autonomous = autonomous.is_some();
            if let Some((constructor, local, owner)) = autonomous {
                let owner = owner.map_err(dom_error)?;
                // Fresh autonomous construction does not push an upgrade
                // element. Recursive constructors and returned alternate
                // instances must retain their actual separate identities.
                let active = active_constructor_scope(&hub, &constructor)?;
                let constructed = ctx.construct_value(constructor.clone(), &[]);
                drop(active);
                let candidate = match constructed {
                    Ok(value) => {
                        constructed_value=Some(value.clone());
                        ctx.with_instance::<DomElement, _>(&value, |element|
                        element.base.realm.resolve_adopted_node(element.base.id))
                        .and_then(|(candidate_realm, candidate)| {
                            let valid = if Rc::ptr_eq(realm, &candidate_realm) {
                                let session = candidate_realm.session.borrow();
                                let document = session.document();
                                matches!(document.kind(candidate), Ok(NodeKind::Element {
                                    namespace: Namespace::Html, attributes, ..
                                }) if attributes.is_empty())
                                    && document.element_name_parts(candidate).ok().is_some_and(|(_, name)| name == local)
                                    && document.first_child(candidate).ok().flatten().is_none()
                                    && document.parent(candidate).ok().flatten().is_none()
                                    && document.node_document(candidate).ok() == Some(owner)
                            } else { false };
                            if valid { Ok(candidate) } else {
                                Err(OpError::new("NotSupportedError", "fresh custom element result has an invalid document, name, attributes, children, or parent"))
                            }
                        })
                    },
                    Err(error) => Err(OpError::thrown(error)),
                };
                match candidate {
                    Ok(candidate) => {
                        realm.session.borrow_mut().document_mut().set_created_element_prefix(candidate,prefix.as_deref()).map_err(dom_error)?;
                        chosen = candidate;
                        set_registry(realm, chosen, Some(&hub))?;
                    }
                    Err(error) => {
                        handle_upgrade_exception_for_constructor(ctx, &constructor, error, true)?;
                        let key = RealmNode::new(realm, id);
                        let mut state = hub.state.borrow_mut();
                        state.failed.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "fresh custom element failed state allocation"))?;
                        state.fresh_fallbacks.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "fresh custom element fallback allocation"))?;
                        state.failed.insert(key);
                        state.fresh_fallbacks.insert(key);
                    }
                }
            } else {
                hub.construct_one(ctx,realm,id,true,true)?;
            }
            if !fresh_autonomous && hub.state.borrow().failed.contains(&RealmNode::new(realm,id)) {
                realm.prepare_allocation(ctx, 1)?;
                chosen=realm.session.borrow_mut().document_mut().create_with_is_value(kind,is_value.as_deref()).map_err(dom_error)?;
                set_registry(realm,chosen,Some(&hub))?;
                let key=RealmNode::new(realm,chosen);let mut state=hub.state.borrow_mut();
                state.failed.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","fresh custom element failure allocation"))?;
                state.fresh_fallbacks.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","fresh custom element fallback allocation"))?;
                state.failed.insert(key);state.fresh_fallbacks.insert(key);
            }
        }
        append_attributes(ctx,chosen)?;Ok(chosen)
    })();
    end_reaction_scope(ctx);
    drop(constructed_value);
    result
}

pub(crate) fn associate_parsed_subtree(realm:&Rc<DomRealm>,root:NodeId,context:NodeId)->OpResult<()> {
    let ambient={let session=realm.session.borrow();parser_birth_registry(realm,session.document(),context)};
    let ids={let session=realm.session.borrow();let document=session.document();let mut ids=Vec::new();let mut cursor=Some(root);
        while let Some(id)=cursor {if ids.len()>document.node_count(){return Err(OpError::new("QuotaExceededError","parsed registry traversal limit"));}ids.push(id);cursor=selector::next_shadow_including_descendant(document,root,id).map_err(dom_error)?;}ids};
    for id in ids {
        let (element,shadow,inert,parent,keep_null)={let session=realm.session.borrow();let document=session.document();
            (matches!(document.kind(id),Ok(NodeKind::Element{..})),document.shadow_host(id).map_err(dom_error)?.is_some(),document.node_document(id).map_err(dom_error).map(|owner|document.is_template_owner_document(owner))?,document.parent(id).map_err(dom_error)?,document.shadow_options(id).ok().flatten().is_some_and(|options|options.keep_custom_element_registry_null))};
        if !element && !shadow {continue;}
        if explicit_registry(realm,id).is_some(){continue;}
        let registry=if inert || keep_null {None} else if shadow {let owner=realm.session.borrow().document().node_document(id).map_err(dom_error)?;registry_hub(realm,owner)}
            else if id==root {ambient.clone()} else {parent.map_or_else(||ambient.clone(),|parent|registry_hub(realm,parent))};
        // Preserve null when the parent has a null association.
        let registry=if element && id!=root && parent.is_some_and(|parent|matches!(explicit_registry(realm,parent),Some(None))) {None}else{registry};
        if registry.as_ref().is_some_and(|hub|!hub.scoped) && explicit_registry(realm,id).is_none(){continue;}
        set_registry(realm,id,registry.as_ref())?;
    }
    Ok(())
}

pub(crate) fn clone_registry_associations(ctx:&mut Ctx,source:&Rc<DomRealm>,target:&Rc<DomRealm>,pairs:&[(NodeId,NodeId)],fallback:Option<Rc<CustomElementHub>>)->OpResult<()> {
    for &(old,new) in pairs {
        let (element,shadow,document_node,inside_cloned_shadow)={let session=source.session.borrow();let document=session.document();
            let mut parent=document.parent(old).ok().flatten();
            let mut inside=false;
            let mut visited=0;
            while let Some(id)=parent {
                visited+=1;
                if visited>document.node_count(){return Err(OpError::new("QuotaExceededError","clone registry ancestor limit"));}
                if document.shadow_host(id).ok().flatten().is_some(){inside=pairs.iter().any(|(source,_)|*source==id);break;}
                parent=document.parent(id).ok().flatten();
            }
            (matches!(document.kind(old),Ok(NodeKind::Element{..})),document.shadow_host(old).ok().flatten().is_some(),matches!(document.kind(old),Ok(NodeKind::Document)),inside)};
        if !element && !shadow && !document_node {continue;}
        let original=registry_hub(source,old);
        let destination_root=target.session.borrow().document().node_document(new).map_err(dom_error)?;
        let global=registry_hub(target,destination_root).filter(|hub|!hub.scoped);
        let registry=if document_node {original.filter(|hub|hub.scoped)}
            else if shadow {original.and_then(|hub|if hub.scoped{Some(hub)}else{global.clone()})}
            else {original.or_else(||(!inside_cloned_shadow).then(||fallback.clone()).flatten()).and_then(|hub|if hub.scoped{Some(hub)}else{global.clone()})};
        // The ordinary matching document global needs no exception entry.
        if registry.as_ref().is_some_and(|hub|global.as_ref().is_some_and(|global|Rc::ptr_eq(hub,global))) {continue;}
        set_registry(target,new,registry.as_ref())?;
    }
    let _=ctx;
    Ok(())
}

pub(crate) fn validate_registry_for_document(ctx:&mut Ctx,realm:&Rc<DomRealm>,document:NodeId,registry:&Value)->OpResult<()> {
    let state = ctx.with_instance::<DomCustomElementRegistry, _>(registry, |registry| registry.hub.state.clone())?;
    let scoped=ctx.with_instance::<DomCustomElementRegistry,_>(registry,|registry|registry.hub.scoped)?;
    if scoped || registry_hub(realm,document).is_some_and(|hub|Rc::ptr_eq(&hub.state,&state)) {
        Ok(())
    } else {
        Err(OpError::new("NotSupportedError", "a global import registry must be the destination document registry"))
    }
}

/// Transfer upgraded custom-element state with adopted nodes and queue the standard lifecycle
/// reaction with the old and new owner documents.
pub(crate) fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    target: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) -> OpResult<()> {
    adopt_nodes_with_documents(ctx,source,target,mapping,&[])
}

/// Logical owner documents can differ from their physical arena roots (for
/// example template contents). Keep registry/state migration in one path.
pub(crate) fn adopt_nodes_with_documents(
    ctx:&mut Ctx,source:&Rc<DomRealm>,target:&Rc<DomRealm>,mapping:&[(NodeId,NodeId)],
    documents:&[(NodeId,NodeId,NodeId)],
) -> OpResult<()> {
    prepare_adoption_publication(source,target)?;
    if Rc::ptr_eq(source,target) {
        let session=source.session.borrow();
        publish_adopted_nodes(source,target,session.document(),session.document(),mapping,documents).map_err(dom_error)?;
    }else {
        let source_session=source.session.borrow();let target_session=target.session.borrow();
        publish_adopted_nodes(source,target,source_session.document(),target_session.document(),mapping,documents).map_err(dom_error)?;
    }
    complete_adopted_nodes(ctx,target)
}

fn adoption_hubs(source:&Rc<DomRealm>)->Result<Vec<Rc<CustomElementHub>>,lumen_html::Error> {
    let mut hubs=Vec::new();let mut seen=HashSet::new();
    let mut add=|hub:Rc<CustomElementHub>|->Result<(),lumen_html::Error> {
        let key=Rc::as_ptr(&hub) as usize;
        if seen.contains(&key) {return Ok(())}
        seen.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
        hubs.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
        seen.insert(key);hubs.push(hub);Ok(())
    };
    if let Some(policy)=source.custom_shadow_policy.borrow().as_ref() {
        for state in policy.registries.borrow().iter().filter_map(std::rc::Weak::upgrade) {
            if let Some(hub)=state.borrow().reactions.owner.upgrade() {add(hub)?;}
        }
    }
    if let Some(hub)=source.custom_element_hub.borrow().upgrade() {add(hub)?;}
    Ok(hubs)
}

/// Attach observers before either parser arena is borrowed.
pub(crate) fn prepare_adoption_publication(source:&Rc<DomRealm>,target:&Rc<DomRealm>)->OpResult<()> {
    for hub in adoption_hubs(source).map_err(dom_error)? {hub.observe_mutations_for(target)?;}
    Ok(())
}

/// One operation-local preflight for the rare multi-owner parser view. Source
/// registry discovery and each registry's attached-owner scan happen once,
/// rather than once for every source/target pair on every resumed feed.
pub(crate) fn prepare_parser_adoption_publication(owners:&[Rc<DomRealm>])->OpResult<()> {
    if owners.len()<2 {return Ok(());}
    let quota=||OpError::new("QuotaExceededError","parser registry preflight allocation limit");
    let mut targets=HashSet::new();let mut seen=HashSet::new();let mut attached=HashSet::new();
    // Charge conservative table storage (keys, control bytes, slack) before
    // admission. Hubs are existing Rc identities; no registry payload is copied.
    let unit=8*core::mem::size_of::<usize>();
    if owners.len().max(4).checked_mul(unit*4).is_none_or(|bytes|bytes>html::MAX_HTML_BYTES) {return Err(quota());}
    targets.try_reserve(owners.len()).map_err(|_|quota())?;
    attached.try_reserve(owners.len()).map_err(|_|quota())?;
    for owner in owners {targets.insert(Rc::as_ptr(owner) as usize);}
    for source in owners {
        let hubs=adoption_hubs(source).map_err(dom_error)?;
        for hub in &hubs {
            let identity=Rc::as_ptr(hub) as usize;
            if seen.contains(&identity) {continue;}
            let prospective=if seen.len()==seen.capacity() {seen.capacity().max(4).checked_mul(2).and_then(|capacity|capacity.checked_add(1)).ok_or_else(quota)?}else {seen.capacity()};
            let bytes=targets.capacity().checked_add(attached.capacity()).and_then(|count|count.checked_add(prospective))
                .and_then(|count|count.checked_mul(unit)).and_then(|bytes|hubs.capacity().checked_mul(core::mem::size_of::<Rc<CustomElementHub>>()).and_then(|hub_bytes|bytes.checked_add(hub_bytes)));
            if bytes.is_none_or(|bytes|bytes>html::MAX_HTML_BYTES) {return Err(quota());}
            seen.try_reserve(1).map_err(|_|quota())?;
            let actual_bytes=targets.capacity().checked_add(attached.capacity()).and_then(|count|count.checked_add(seen.capacity()))
                .and_then(|count|count.checked_mul(unit)).and_then(|bytes|hubs.capacity().checked_mul(core::mem::size_of::<Rc<CustomElementHub>>()).and_then(|hub_bytes|bytes.checked_add(hub_bytes)));
            if actual_bytes.is_none_or(|bytes|bytes>html::MAX_HTML_BYTES) {return Err(quota());}
            seen.insert(identity);attached.clear();
            {
                let mut realms=hub.attached_realms.borrow_mut();realms.retain(|realm|realm.strong_count()!=0);
                for realm in realms.iter().filter_map(std::rc::Weak::upgrade) {
                    let identity=Rc::as_ptr(&realm) as usize;if targets.contains(&identity) {attached.insert(identity);}
                }
            }
            for target in owners {
                let identity=Rc::as_ptr(target) as usize;
                if attached.contains(&identity) {continue;}
                hub.observe_mutations_for(target)?;attached.insert(identity);
            }
        }
    }
    Ok(())
}

/// Publish fixed state before insertion. Uses supplied documents and creates no
/// JS wrappers or callbacks, so destination observers see the authoritative state.
pub(crate) fn publish_adopted_nodes(
    source:&Rc<DomRealm>,target:&Rc<DomRealm>,source_document:&lumen_html::Document,target_document:&lumen_html::Document,
    mapping:&[(NodeId,NodeId)],documents:&[(NodeId,NodeId,NodeId)],
)->Result<(),lumen_html::Error> {
    let mut associations=Vec::new();
    for (index, &(old,new)) in mapping.iter().enumerate() {
        let owners=documents.get(index).filter(|entry|entry.0==old)
            .or_else(||documents.iter().find(|entry|entry.0==old));
        if Rc::ptr_eq(source,target) && owners.is_some_and(|entry|entry.1==entry.2) {continue;}
        let original=explicit_registry(source,old).unwrap_or_else(||registry_hub_for_document(source,source_document,old).or_else(||source.custom_element_hub.borrow().upgrade()));
        let (element,shadow,parent,keep_null,owner,exclusive_parent)={let document=target_document;
            let parent=document.parent(new).ok().flatten();
            let exclusive_parent=parent.is_some_and(|parent|matches!(document.kind(parent),Ok(NodeKind::DocumentFragment))&&document.shadow_host(parent).ok().flatten().is_none());
            (matches!(document.kind(new),Ok(NodeKind::Element{..})),document.shadow_host(new).ok().flatten().is_some(),parent,
                document.shadow_options(new).ok().flatten().is_some_and(|options|options.keep_custom_element_registry_null),
                document.node_document(new)?,exclusive_parent)};
        if !element && !shadow {continue;}
        let registry=if original.as_ref().is_some_and(|hub|hub.scoped) {original}
            else if shadow {
                if original.is_none() && keep_null {None} else {registry_hub_for_document(target,target_document,owner).filter(|hub|!hub.scoped)}
            } else {
                let basis=if original.is_some() || parent.is_none() || exclusive_parent {owner}else{parent.unwrap_or(owner)};
                registry_hub_for_document(target,target_document,basis).filter(|hub|!hub.scoped)
            };
        associations.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
        associations.push((old,new,registry));
    }
    let hubs=adoption_hubs(source)?;
    for hub in &hubs {
        let mut state = hub.state.borrow_mut();
        let mut upgraded = 0;
        let mut connected = 0;
        let mut failed = 0;
        for &(old, _) in mapping {
            let key = RealmNode::new(source, old);
            upgraded += usize::from(state.upgraded.contains_key(&key));
            connected += usize::from(state.connected.contains_key(&key));
            failed += usize::from(state.failed.contains(&key));
        }
        state.upgraded.try_reserve(upgraded).map_err(|_|lumen_html::Error::LimitExceeded)?;
        state.connected.try_reserve(connected).map_err(|_|lumen_html::Error::LimitExceeded)?;
        state.form_states.try_reserve(upgraded).map_err(|_|lumen_html::Error::LimitExceeded)?;
        state.failed.try_reserve(failed).map_err(|_|lumen_html::Error::LimitExceeded)?;
        state.fresh_fallbacks.try_reserve(failed).map_err(|_|lumen_html::Error::LimitExceeded)?;
        state.internals_shadow_roots.try_reserve(upgraded).map_err(|_|lumen_html::Error::LimitExceeded)?;
    }
    // Plain publication cannot fail midway and leave fallback migration reading
    // partially moved associations. Admit each actual sparse allocation first.
    if !associations.is_empty() {
        let mut stored=target.custom_shadow_policy.borrow_mut();
        let policy=stored.get_or_insert_with(||Rc::new(ShadowPolicy{primary:RefCell::new(std::rc::Weak::new()),registries:RefCell::new(Vec::new()),associations:RefCell::new(HashMap::new())}));
        let mut existing=policy.associations.borrow_mut();
        let additional=|existing:&HashMap<NodeId,Option<std::rc::Weak<CustomElementHub>>>|associations.iter().filter(|(_,node,_)|!existing.contains_key(node)).count();
        if existing.len().saturating_add(additional(&existing))>MAX_REGISTRY_ENTRIES {existing.retain(|node,_|target_document.kind(*node).is_ok());}
        let count=additional(&existing);
        if existing.len().saturating_add(count)>MAX_REGISTRY_ENTRIES{return Err(lumen_html::Error::LimitExceeded)}
        existing.try_reserve(count).map_err(|_|lumen_html::Error::LimitExceeded)?;
    }
    let mut scoped_owners=HashMap::new();
    for (_,node,hub) in &associations {
        if let Some(hub)=hub.as_ref().filter(|hub|hub.scoped) {
            let owner=target_document.node_document(*node)?;let key=(Rc::as_ptr(hub) as usize,owner);
            if !scoped_owners.contains_key(&key) {
                scoped_owners.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;
                scoped_owners.insert(key,hub.clone());
            }
        }
    }
    let mut scoped_counts=HashMap::<usize,(Rc<CustomElementHub>,usize)>::new();
    for ((pointer,owner),hub) in scoped_owners {
        let mut documents=hub.scoped_documents.borrow_mut();documents.retain(|(realm,_)|realm.strong_count()!=0);
        if documents.iter().any(|(realm,node)|*node==owner && realm.ptr_eq(&Rc::downgrade(target))) {continue}
        if !scoped_counts.contains_key(&pointer) {scoped_counts.try_reserve(1).map_err(|_|lumen_html::Error::LimitExceeded)?;scoped_counts.insert(pointer,(hub.clone(),0));}
        scoped_counts.get_mut(&pointer).unwrap().1+=1;
    }
    for (_, (hub,count)) in scoped_counts {
        let mut documents=hub.scoped_documents.borrow_mut();
        if documents.len().saturating_add(count)>MAX_REGISTRY_ENTRIES{return Err(lumen_html::Error::LimitExceeded)}
        documents.try_reserve(count).map_err(|_|lumen_html::Error::LimitExceeded)?;
    }
    let custom_count=hubs.iter().map(|hub|{let state=hub.state.borrow();mapping.iter().filter(|(old,_)|state.upgraded.contains_key(&RealmNode::new(source,*old))).count()}).sum::<usize>();
    target.retained_nodes.borrow_mut().try_reserve(custom_count.saturating_mul(2)).map_err(|_|lumen_html::Error::LimitExceeded)?;
    source.retained_nodes.borrow_mut().try_reserve(custom_count).map_err(|_|lumen_html::Error::LimitExceeded)?;
    for (old,new,registry) in associations {
        remember_registry(target,target_document,new,registry.as_ref())?;
        if !Rc::ptr_eq(source,target)||old!=new {
            if let Some(policy)=source.custom_shadow_policy.borrow().as_ref(){policy.associations.borrow_mut().remove(&old);}
        }
    }
    let old_root=source_document.root();
    let new_root=target_document.root();
    for hub in hubs {

    {
        let mut state = hub.state.borrow_mut();
        for (index,&(old, new)) in mapping.iter().enumerate() {
            let old_key = RealmNode::new(source, old);
            let new_key = RealmNode::new(target, new);
            let definition_name = state.upgraded.remove(&old_key);
            if let Some(name) = definition_name.as_ref() {
                state.upgraded.insert(new_key, name.clone());
            }
            if let Some(connected) = state.connected.remove(&old_key) {
                state.connected.insert(new_key, connected);
            }
            if state.failed.remove(&old_key) {
                state.failed.insert(new_key);
            }
            if state.fresh_fallbacks.remove(&old_key) {state.fresh_fallbacks.insert(new_key);}
            state.reactions.rebase(old_key, new_key);
            if let Some((_,disabled))=state.form_states.remove(&old_key){state.form_states.insert(new_key,(None,disabled));}
            if state.internals_shadow_roots.remove(&old_key) { state.internals_shadow_roots.insert(new_key); }
            let owners=documents.get(index).filter(|entry|entry.0==old)
                .or_else(||documents.iter().find(|entry|entry.0==old));
            let (old_owner,new_owner)=owners.map_or((old_root,new_root),|entry|(entry.1,entry.2));
            if !Rc::ptr_eq(source,target)||old_owner!=new_owner {
            if let Some(callback) = definition_name
                .as_ref()
                .and_then(|name| state.definitions.get(name))
                .and_then(|definition| definition.callbacks.get("adoptedCallback"))
                .cloned()
            {
                state.reactions.push_back(Reaction::AdoptedPending(
                    new_key,
                    callback,
                    AdoptedDocument::new(source,old_owner),
                    AdoptedDocument::new(target,new_owner),
                ));

            }
            }
        }
    }
    }
    Ok(())
}

/// Complete after all arena borrows and native wrapper migration end.
pub(crate) fn complete_adopted_nodes(ctx:&mut Ctx,target:&Rc<DomRealm>)->OpResult<()> {
    for hub in adoption_hubs(target).map_err(dom_error)? {
        let pending={
            let state=hub.state.borrow();
            let count=state.reactions.queues.values().flat_map(|queue|queue.iter())
                .filter(|reaction|matches!(reaction,Reaction::AdoptedPending(..))).count();
            let mut pending=Vec::new();pending.try_reserve(count)
                .map_err(|_|OpError::new("QuotaExceededError","adopted callback argument admission"))?;
            for (key,queue) in &state.reactions.queues {
                for (index,reaction) in queue.iter().enumerate() {
                    if matches!(reaction,Reaction::AdoptedPending(..)) {pending.push((*key,index,reaction.clone()));}
                }
            }
            pending
        };
        // Native wrapper allocation/identity registration may trace registry
        // state. Materialize outside its RefCell borrow; no author code runs,
        // and existing queue positions retain their original reaction order.
        for (owner,index,reaction) in pending {
            if let Reaction::AdoptedPending(key,callback,old,new)=reaction {
                let replacement=Reaction::Adopted(key,callback,old.value(ctx),new.value(ctx));
                if let Some(reaction)=hub.state.borrow_mut().reactions.queues.get_mut(&owner).and_then(|queue|queue.get_mut(index)) {
                    if matches!(reaction,Reaction::AdoptedPending(..)) {*reaction=replacement;}
                }
            }
        }
        let session=target.session.borrow();let mut state=hub.state.borrow_mut();
        refresh_form_associations(&mut state,session.document(),target);
        let queued=state.reactions.len()!=0;drop(state);drop(session);
        if queued {hub.schedule();}
    }
    Ok(())
}

pub(crate) fn upgrade_created_element(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    id: NodeId,
) -> OpResult<()> {
    if realm.session.borrow().document().node_document(id).ok()
        .is_some_and(|owner| realm.session.borrow().document().is_template_owner_document(owner)) {
        return Ok(());
    }
    if let Some(hub) = registry_hub(realm,id) {hub.upgrade_subtree(ctx, realm, id)?;}
    Ok(())
}

fn handle_upgrade_exception(ctx: &mut Ctx, error: OpError, report: bool) -> OpResult<()> {
    if report {
        let exception = error.to_value(ctx);
        crate::error_reporting::report_exception(ctx, exception);
        Ok(())
    } else { Err(error) }
}

fn handle_upgrade_exception_for_constructor(ctx: &mut Ctx, constructor: &Value, error: OpError, report: bool) -> OpResult<()> {
    if !report { return Err(error); }
    let realm = JsFunction::from_value(constructor.clone()).and_then(|function| ctx.function_host_realm(&function).ok());
    if let Some(realm) = realm {
        let exception = error.to_value(ctx);
        // Reporting is associated with the definition's constructor global,
        // which can differ from the registry's owning global.
        ctx.with_host_realm(&realm, |ctx| crate::error_reporting::report_exception(ctx, exception))
            .map_err(|_| OpError::new("InvalidStateError", "custom element constructor realm is unavailable"))?;
        Ok(())
    } else { handle_upgrade_exception(ctx, error, true) }
}

pub(crate) fn upgrade_cloned_or_parsed_subtree(_ctx: &mut Ctx, realm: &Rc<DomRealm>, id: NodeId) -> OpResult<()> {
    {
        let session = realm.session.borrow();
        if session.document().node_document(id).ok().is_some_and(|owner| session.document().is_template_owner_document(owner)) {
            return Ok(());
        }
    }
    let hubs:Vec<_>=realm.custom_shadow_policy.borrow().as_ref().map_or_else(Vec::new,|policy|policy.registries.borrow().iter().filter_map(std::rc::Weak::upgrade).filter_map(|state|state.borrow().reactions.owner.upgrade()).collect());
    for hub in hubs {hub.enqueue_upgrade_subtree(realm,id,None)?;}
    Ok(())
}

#[cfg(test)]
pub(crate) fn deliver_reactions(ctx: &mut Ctx) -> OpResult<()> {
    if let Some(hub) = hub_from_ctx(ctx) {
        hub.flush(ctx)?;
    }
    Ok(())
}

/// Called by the `HTMLElement` binding's constructor. It identifies the active class through
/// `new.target`, consumes the upgrade stack when present, or creates a detached autonomous
/// element for direct `new MyElement()` construction.
pub(crate) fn construct_html_element(ctx: &mut Ctx, this: Value) -> OpResult<HtmlElementCtor> {
    construct_for_superclass(ctx, this, None)
}

pub(crate) fn construct_customized_element(
    ctx: &mut Ctx,
    this: Value,
    expected_tag: &str,
) -> OpResult<HtmlElementCtor> {
    let expected = custom_name_tag(expected_tag).ok_or_else(|| OpError::new("TypeError", "unsupported HTML superclass"))?;
    construct_for_superclass(ctx, this, Some(expected))
}

pub(crate) fn construct_customized_interface(ctx: &mut Ctx, this: Value, expected_interface: &str) -> OpResult<HtmlElementCtor> {
    construct_for_superclass(ctx, this, Some(expected_interface))
}

pub(crate) fn construct_customized_class<T: lumen_bind::Class>(ctx: &mut Ctx, this: Value) -> OpResult<HtmlElementCtor> {
    construct_customized_interface(ctx, this, T::DESC.name_for("js"))
}

fn construct_for_superclass(
    ctx: &mut Ctx,
    this: Value,
    expected_interface: Option<&str>,
) -> OpResult<HtmlElementCtor> {
    let target = ctx.current_new_target();
    // HTMLConstructor step 1 compares function identities, not prototypes or
    // superclass chains. A proxy used as new.target remains a distinct target.
    let active = super::html_interfaces::interface_constructor(ctx, expected_interface.unwrap_or("HTMLElement"))
        .map_err(OpError::thrown)?;
    if ctx.values_strict_equal(&target, &active) {
        return Err(OpError::type_error("HTML interface constructor requires a custom new.target"));
    }
    let agent = agent_reactions(ctx);
    let active_hub = agent.borrow().active_constructors.iter().rev()
        .find(|(constructor, _)| ctx.values_strict_equal(constructor, &target))
        .and_then(|(_, hub)| hub.upgrade()).map(|hub| (*hub).clone());
    let hub = active_hub.or_else(|| hub_from_ctx(ctx))
        .ok_or_else(|| OpError::new("TypeError", "custom element registry is unavailable"))?;
    let (name, extends) = hub
        .constructor_for_new_target(ctx, &target)
        .ok_or_else(|| OpError::new("TypeError", "Illegal constructor"))?;
    if extends.as_deref().and_then(custom_name_tag).unwrap_or("HTMLElement") != expected_interface.unwrap_or("HTMLElement") {
        return Err(OpError::new(
            "TypeError",
            "custom element called the wrong HTML superclass constructor",
        ));
    }
    // HTMLConstructor performs its registry and superclass checks before this
    // observable lookup. GetFunctionRealm follows proxy/bound targets.
    let prototype = ctx.member_get(&target, "prototype").map_err(OpError::thrown)?;
    let prototype = if matches!(prototype, Value::Obj(_)) {
        prototype
    } else {
        let function = lumen::embed::JsFunction::from_value(target)
            .ok_or_else(|| OpError::new("TypeError", "new.target is not a constructor"))?;
        let target_realm = ctx.function_host_realm(&function).map_err(OpError::thrown)?;
        ctx.with_host_realm(&target_realm, |ctx| {
            super::html_interfaces::interface_prototype(ctx, expected_interface.unwrap_or("HTMLElement"))
        }).map_err(|_| OpError::new("TypeError", "constructor realm is unavailable"))?
            .map_err(OpError::thrown)?
    };
    let pending = hub.consume_pending_upgrade(&name)?;
    let (realm, id, upgrading) = if let Some((realm, id)) = pending {
        (realm, id, true)
    } else {
        // The HTML superclass's current global supplies the associated
        // Document; the active registry can belong to another document.
        let realm = hub_from_ctx(ctx).map(|current| current.realm.clone())
            .ok_or_else(|| OpError::new("TypeError", "HTML constructor document is unavailable"))?;
        realm.prepare_allocation(ctx,1)?;
        let id = realm
            .session
            .borrow_mut()
            .document_mut()
            .create_with_is_value(NodeKind::Element {
                namespace: Namespace::Html,
                name: extends.as_deref().unwrap_or(&name).into(),
                attributes: Vec::new(),
            }, extends.as_ref().map(|_| name.as_str()))
            .map_err(dom_error)?;
        (realm, id, false)
    };
    if !upgrading {
        let owner=hub.state.borrow().reactions.owner.upgrade();
        if let Some(owner)=owner {set_registry(&realm,id,Some(&owner))?;}
        if let Err(error) = hub.mark_upgraded(&realm, id, name.clone()) {
            let _ = realm.session.borrow_mut().document_mut().destroy_subtree(id);
            return Err(error);
        }
    }
    if !upgrading {hub.complete_form_element(&realm,id)?;}
    Ok(HtmlElementCtor {
        this,
        prototype,
        realm,
        id,
        custom_name: Some(name),
        interface: extends,
        upgrading,
    })
}

pub(crate) fn custom_element_is(ctx: &mut Ctx, options: Option<Value>) -> OpResult<Option<String>> {
    let Some(options) = options else {
        return Ok(None);
    };
    match options {
        Value::Undefined | Value::Null => Ok(None),
        // The legacy string overload is retained for compatibility but does
        // not supply the creation dictionary's `is` value.
        Value::Str(_) => Ok(None),
        value => {
            let is = ctx.member_get(&value, "is").map_err(OpError::thrown)?;
            if matches!(is, Value::Undefined) {
                Ok(None)
            } else {
                Ok(Some(
                    ctx.coerce_string(&is).map_err(OpError::thrown)?.to_string(),
                ))
            }
        }
    }
}

fn custom_name_tag(tag: &str) -> Option<&'static str> {
    super::html_interfaces::custom_interface(tag).or_else(||
        (lumen_html::html::classify_html_element_name(tag) == lumen_html::html::HtmlElementNameKind::BuiltIn)
            .then_some("HTMLElement"))
}

fn tree_connected(document: &lumen_html::Document, id: NodeId) -> bool {
    let root = document.root();
    let mut current = id;
    loop {
        if current == root {
            return true;
        }
        match document.shadow_including_parent(current) {
            Ok(Some(parent)) => current = parent,
            _ => return false,
        }
    }
}

fn valid_name(name: &str) -> bool {
    lumen_html::html::is_valid_custom_element_name(name)
}

/// Host-specific constructor result used by `HTMLElement`'s native binding. The concrete binding
/// hook is intentionally kept in `lib.rs`; this builder is the point where the engine's
/// superclass-return substitution preserves a wrapper's Node/EventTarget identity during upgrade.
pub(crate) struct HtmlElementCtor {
    pub(crate) this: Value,
    pub(crate) prototype: Value,
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) id: NodeId,
    pub(crate) custom_name: Option<String>,
    pub(crate) interface: Option<String>,
    pub(crate) upgrading: bool,
}

impl HtmlElementCtor {
    pub(crate) fn into_ctor_for(self, ctx: &mut Ctx, expected_interface: &str) -> Result<Value, Value> {
        let actual_interface = self
            .interface
            .as_deref()
            .and_then(custom_name_tag)
            .unwrap_or("HTMLElement");
        if actual_interface != expected_interface {
            return Err(ctx.make_error(
                "TypeError",
                "custom element called the wrong HTML superclass constructor",
            ));
        }
        let Some(_name) = self.custom_name else {
            return Err(ctx.make_error("TypeError", "Illegal constructor"));
        };
        // During upgrade `wrap` returns the pre-existing wrapper; keep its native data and
        // target listeners, then swap only its prototype before returning it from `super()`.
        let has_super_receiver = matches!(self.this, Value::Obj(_));
        let result = (|| {
        let value = if self.upgrading {
            let wrapper = self.realm.wrap(ctx, self.id);
            set_html_prototype(ctx, &wrapper, &self.prototype)?;
            wrapper
        } else {
            let value = if has_super_receiver { self.this.clone() } else { ctx.new_object_with_proto(&self.prototype) };
            if has_super_receiver { set_html_prototype(ctx, &value, &self.prototype)?; }
            attach_html_native(ctx, &value, &self.realm, self.id, expected_interface)?;
            ctx.set_native_identity_owner::<super::DomNode>(&value)
                .map_err(|error| error.to_value(ctx))?;
            let weak = ctx.weak_value(&value).ok_or_else(|| {
                ctx.make_error("TypeError", "HTMLElement instance is not an object")
            })?;
            self.realm.wrappers.borrow_mut().insert(self.id, weak);
            value
        };
        if has_super_receiver {
            ctx.replace_current_native_super_result(value.clone())?;
        }
        Ok(value)
        })();
        if result.is_err() && !self.upgrading {
            self.realm.wrappers.borrow_mut().remove(&self.id);
            if let Some(hub) = hub_from_ctx(ctx) {
                let key = RealmNode::new(&self.realm, self.id);
                let mut state = hub.state.borrow_mut();
                state.upgraded.remove(&key);
                state.connected.remove(&key);
                state.failed.remove(&key);
                state.reactions.retain(|reaction| reaction.node() != key);
            }
            let _ = self.realm.session.borrow_mut().document_mut().destroy_subtree(self.id);
        }
        result
    }
}

macro_rules! custom_ctor_ret {
    ($ty:ty, $interface:literal) => {
        impl CtorRet<JsHost, $ty> for HtmlElementCtor {
            fn into_ctor(self, cx: &lumen::embed::ArgCx<'_>) -> Result<Value, Value> {
                <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| self.into_ctor_for(ctx, $interface))
            }
        }
    };
}

custom_ctor_ret!(DomHtmlElement, "HTMLElement");

fn attach_html_native(
    ctx: &mut Ctx,
    target: &Value,
    realm: &Rc<DomRealm>,
    id: NodeId,
    interface: &str,
) -> Result<(), Value> {
    let html = || DomHtmlElement {
        base: DomElement {
            base: DomNode {
                base: DomEventTarget::node(realm, id),
                realm: realm.clone(),
                id,
                collections: RefCell::new(HashMap::new()),
            },
        },
    };
    if interface == "HTMLElement" {
        return ctx.attach_native_data(target, html()).map_err(|error| error.into_error(ctx));
    }
    if super::html_interfaces::attach_custom_interface(ctx, target, html(), interface)? {
        Ok(())
    } else {
        Err(ctx.make_error("TypeError", "unsupported custom element base interface"))
    }
}

fn set_html_prototype(ctx: &mut Ctx, target: &Value, prototype: &Value) -> Result<(), Value> {
    if ctx.reflect_set_prototype_of(target, prototype)? {
        Ok(())
    } else {
        Err(ctx.make_error("TypeError", "HTML constructor cannot set the instance prototype"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_default_aria_reflection_scopes_weak_targets_and_caches_frozen_arrays() {
        let mut engine=Engine::new();let _realm=crate::install(engine.ctx(),"<main id='host'></main><span id='reference'></span>",1024).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(v,label)=>{if(!v)throw Error(label)};const callbacks=[];
            class Aria extends HTMLElement {static observedAttributes=['aria-label'];constructor(){super();this.i=this.attachInternals()}attributeChangedCallback(name,old,value){callbacks.push(value)}}
            customElements.define('x-aria',Aria);const element=new Aria();document.getElementById('host').append(element);globalThis.ariaTarget=element;
            const i=element.i;check(i.role===null && i.ariaLabel===null && i.ariaControlsElements===null,'null defaults');
            i.role='button';i.ariaLabel={toString(){return 'default'}};
            check(i.role==='button' && i.ariaLabel==='default' && !element.hasAttribute('aria-label') && callbacks.length===0,'internal defaults are separate from author attributes');
            element.ariaLabel='author';check(element.getAttribute('aria-label')==='author' && callbacks.join(',')==='author','Element reflection and CE reactions');
            check(i.ariaLabel==='default','author override does not overwrite default');
            element.ariaLabel=null;check(!element.hasAttribute('aria-label'),'nullable reflection removes attribute');
            const reference=document.getElementById('reference');
            i.ariaActiveDescendantElement=reference;check(i.ariaActiveDescendantElement===reference,'associated element in ancestor scope');
            const input=[reference,document.body];i.ariaControlsElements=input;input.length=0;
            const result=i.ariaControlsElements;check(result.length===2 && result[0]===reference && Object.isFrozen(result) && result===i.ariaControlsElements,'converted frozen array and cached identity');
            let marker={};let caught;try{i.ariaControlsElements={[Symbol.iterator](){return{next(){throw marker}}}}}catch(e){caught=e}
            check(caught===marker && i.ariaControlsElements===result,'abrupt sequence conversion is atomic');
            const root=element.attachShadow({mode:'open'});const inner=document.createElement('span');root.append(inner);
            i.ariaActiveDescendantElement=inner;check(i.ariaActiveDescendantElement===null,'outer target cannot reference descendant shadow tree');
            reference.remove();check(i.ariaControlsElements!==result && i.ariaControlsElements.length===1,'tree mutation updates scoped array');
            i.ariaControlsElements=null;check(i.ariaControlsElements===null,'null removes explicit references');
            return true;
        })()"#),Value::Bool(true)));
        let element=eval(&mut engine,"ariaTarget");
        assert!(matches!(effective_aria_attribute(engine.ctx(),&element,"aria-label").ok(),Some(Value::Str(value)) if value.as_str()=="default"));
        eval(&mut engine,"ariaTarget.setAttribute('aria-label','')");
        assert!(matches!(effective_aria_attribute(engine.ctx(),&element,"aria-label").ok(),Some(Value::Str(value)) if value.as_str().is_empty()));
    }

    #[test]
    fn specification_face_restore_snapshots_strings_files_and_entry_lists_without_live_roots() {
        const HTML:&str="<form><x-restored name='one'></x-restored><x-restored name='two'></x-restored><x-restored name='three'></x-restored></form>";
        const DEFINE:&str=r#"class Restored extends HTMLElement {
            static formAssociated=true;
            constructor(){super();this.i=this.attachInternals();this.i.setFormValue('default')}
            formStateRestoreCallback(state,reason){this.restored=state;this.reason=reason;this.i.setFormValue(state)}
        };customElements.define('x-restored',Restored);globalThis.controls=[...document.querySelectorAll('x-restored')];"#;
        let mut source=Engine::new();assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(source.ctx()).is_ok());let source_realm=crate::install(source.ctx(),HTML,1024).unwrap();eval(&mut source,DEFINE);
        eval(&mut source,r#"controls[0].i.setFormValue('submission','saved string');
            const file=new File(['file bytes'],'state.txt',{type:'text/plain',lastModified:42});
            controls[1].i.setFormValue(null,file);
            const data=new FormData();data.append('name','value');data.append('file',file);controls[2].i.setFormValue('other',data);"#);
        let snapshot=snapshot_custom_form_state(source.ctx(),&source_realm).unwrap_or_else(|_|panic!("custom form state snapshot failed"));
        assert_eq!(snapshot.len(),3);
        let mut target=Engine::new();assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(target.ctx()).is_ok());let target_realm=crate::install(target.ctx(),HTML,1024).unwrap();eval(&mut target,DEFINE);
        restore_custom_form_state(target.ctx(),&target_realm,&snapshot).unwrap_or_else(|_|panic!("custom form state restoration failed"));
        assert!(matches!(eval(&mut target,r#"controls[0].restored==='saved string' && controls.every(c=>c.reason==='restore') &&
            controls[1].restored instanceof File && controls[1].restored.name==='state.txt' && controls[1].restored.lastModified===42 &&
            controls[2].restored instanceof FormData && controls[2].restored.get('name')==='value' && controls[2].restored.get('file') instanceof File"#),Value::Bool(true)));
    }

    #[test]
    fn specification_custom_states_share_live_ordered_storage_selector_dependencies_and_target_retention() {
        let mut engine=Engine::new();let _realm=crate::install(engine.ctx(),"<style>x-states:state(ready){color:rgb(1, 2, 3)}</style><main id='host'></main>",1024).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(v,label)=>{if(!v)throw Error(label)};
            class States extends HTMLElement {constructor(){super();this.i=this.attachInternals()}}
            customElements.define('x-states',States);
            const element=new States();document.getElementById('host').append(element);
            const states=element.i.states;
            check(states===element.i.states && states instanceof CustomStateSet,'SameObject typed state set');
            check(states.add('')===states && states.add('a')===states,'DOMString state set accepts arbitrary strings');
            states.add('b');states.add('c');states.add('a');
            const iterator=states.values();check(iterator.next().value==='','insertion order');
            states.delete('a');check(iterator.next().value==='b','delete next does not shift cursor');
            states.add('a');check(iterator.next().value==='c' && iterator.next().value==='a','readd appends');
            check(iterator.next().done,'done');states.add('d');check(iterator.next().done,'finished iterator stays finished');
            states.clear();const visited=[];
            states.add('first');states.forEach(function(value,key,owner){check(this===element && value===key && owner===states,'forEach arguments');visited.push(value);if(value==='first')states.add('later')},element);
            check(visited.join(',')==='first,later','live mutation iteration');
            states.add('ready');check(element.matches(':state(ready)') && document.querySelector('x-states:state(ready)')===element,'canonical selector parser');
            check(getComputedStyle(element).color==='rgb(1, 2, 3)','state style dependency');
            states.delete('ready');check(!element.matches(':state(ready)') && getComputedStyle(element).color!=='rgb(1, 2, 3)','mutation invalidates style');
            states.add('(escaped state');check(element.matches(':state(\\(escaped\\ state)'),'escaped identifier');
            check(element.matches(':not(:hover):state(later)') && element.matches(':state(later):not(:hover)'), 'functional state retains adjacent pseudo boundaries');
            const canonical=new CSSStyleSheet();
            canonical.replaceSync('x-states:state(  ready  ) {color:red}');
            check(canonical.cssRules[0].selectorText==='x-states:state(ready)','shared selector CSSOM canonical state argument');
            canonical.cssRules[0].selectorText='x-states:is(:state( ready ))';
            check(canonical.cssRules[0].selectorText==='x-states:is(:state(ready))','nested authored state CSSOM serialization');
            const shadowHost=document.createElement('div');shadowHost.id='state-part-host';
            document.body.append(shadowHost);const shadow=shadowHost.attachShadow({mode:'open'});
            const part=new States();part.setAttribute('part','indicator');shadow.append(part);
            canonical.replaceSync('#state-part-host::part(indicator):state(ready) {color:rgb(4, 5, 6)}');
            document.adoptedStyleSheets.push(canonical);
            part.i.states.add('ready');check(getComputedStyle(part).color==='rgb(4, 5, 6)','host ID part rule is indexed for exposed child state');
            part.i.states.delete('ready');check(getComputedStyle(part).color!=='rgb(4, 5, 6)','part state dependency invalidates actual exposed cascade');
            for(const invalid of [':state',':state()',':state(a b)',':state(16px)']){let caught=false;try{element.matches(invalid)}catch(e){caught=e.name==='SyntaxError'}check(caught,'invalid state grammar '+invalid)}
            const clone=element.cloneNode();check(clone.i.states.size===0,'clone state isolated');
            element.remove();states.add('retained');check(states.has('retained'),'retained state set keeps target usable');
            return true;
        })()"#),Value::Bool(true)));
    }

    #[test]

    fn specification_element_internals_form_values_validity_labels_and_reactions_share_live_control_state() {
        let mut engine=Engine::new();assert!(lumen_host::lazy_globals::<lumen_host::blob::bindings::Module>(engine.ctx()).is_ok());let _realm=crate::install(engine.ctx(),"<form id='f'><input name='before' value='a'><fieldset id='fs'><legend id='legend'></legend><label id='lab' for='custom'>Custom</label><x-face id='custom' name='face'></x-face></fieldset><input name='after' value='z'></form>",1024).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(v,label)=>{if(!v)throw Error(label)};
            const events=[];
            class Face extends HTMLElement {
                static formAssociated=true;
                constructor(){super();this.internals=this.attachInternals();this.internals.setFormValue('initial')}
                formAssociatedCallback(form){events.push(['form',form])}
                formDisabledCallback(value){events.push(['disabled',value])}
                formResetCallback(){events.push(['reset']);this.internals.setFormValue('reset')}
            }
            customElements.define('x-face',Face);
            const c=document.getElementById('custom'), f=document.getElementById('f'),fs=document.getElementById('fs'),i=c.internals;
            check(i.form===f && f.elements.namedItem('face')===c,'shared listed control/form owner');
            check(i.labels===i.labels && i.labels.length===1 && i.labels[0].id==='lab','live same-object labels');
            check(events.length===1 && events[0][0]==='form' && events[0][1]===f,'upgrade form association callback');
            const file=new File(['data'],'kept.txt');const data=new FormData();data.append('one','1');data.append('file',file);
            i.setFormValue(data,null);data.append('late','not submitted');
            const entries=[...new FormData(f)];
            check(entries.map(x=>x[0]).join(',')==='before,one,file,after','snapshotted entry list and document order');
            check(entries[2][1]===file,'File identity retained');
            i.setFormValue(null);check([...new FormData(f)].map(x=>x[0]).join(',')==='before,after','null suppresses submission');
            i.setValidity();check(i.validity.valid,'default validity dictionary binding');
            const validity=i.validity;let error='';try{i.setValidity({valueMissing:true})}catch(e){error=e.name}
            check(error==='TypeError' && validity.valid,'missing message leaves flags untouched');
            i.setValidity({customError:true},null);check(i.validationMessage==='null','DOMString null message conversion');
            i.setValidity({valueMissing:true},'line\r\nnext\rtail');
            check(i.validity===validity && validity.valueMissing && i.validationMessage==='line\nnext\ntail','live validity and normalized message');
            let invalid=0;c.addEventListener('invalid',e=>{invalid++;e.preventDefault()});
            check(!i.checkValidity() && invalid===1 && !i.reportValidity() && invalid===2,'one invalid event per validation');
            check(!f.reportValidity() && invalid===3,'form interaction mutation follows shared eligibility snapshot');
            try{i.setValidity({customError:true},'changed',f)}catch(e){error=e.name}
            check(error==='NotFoundError' && validity.customError && !validity.valueMissing && i.validationMessage==='changed','flags applied before anchor validation');
            c.setAttribute('readonly','');check(!i.willValidate && i.checkValidity() && i.validationMessage==='','readonly bars validation message');c.removeAttribute('readonly');
            check(i.validationMessage==='changed','removing the validation bar preserves stored message');
            fs.disabled=true;check(!i.willValidate && i.validationMessage==='','fieldset bars validation message');
            check(events.at(-1)[0]==='disabled' && events.at(-1)[1]===true,'fieldset disabled callback');
            document.getElementById('legend').append(c);
            check(i.willValidate && i.form===f,'first legend disabled exemption');
            check(events.filter(event=>event[0]==='disabled').at(-1)[1]===false,'first legend disabled callback');
            check(i.validationMessage==='changed','legend exemption reveals stored message');
            f.reset();check(events.at(-1)[0]==='reset' && new FormData(f).get('face')==='reset','reset callback controls submission state');
            const clone=c.cloneNode();check(clone.internals!==i && clone.internals.validity.valid,'clone state isolated');
            class Ordinary extends HTMLElement {constructor(){super();this.i=this.attachInternals()}}
            customElements.define('x-ordinary',Ordinary);const ordinary=new Ordinary();error='';try{ordinary.i.setFormValue('x')}catch(e){error=e.name}
            check(error==='NotSupportedError','non form-associated target rejected');return true;
        })()"#),Value::Bool(true)));
    }

    #[test]
    fn scoped_registry_documents_initialization_and_internals_preserve_fixed_associations() {
        let mut engine=Engine::new();let _realm=crate::install(engine.ctx(),r#"<main id='host'><template shadowrootmode='closed' shadowrootclonable shadowrootcustomelementregistry><x-inner-case></x-inner-case></template></main>"#,256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(value,label)=>{if(!value)throw Error(label)};
            const unsupported=fn=>{let name='';try{fn()}catch(error){name=error.name}check(name==='NotSupportedError','NotSupportedError')};
            check(document.customElementRegistry===customElements,'Document global registry');
            const detached=new Document();check(detached.customElementRegistry===null,'new Document null registry');
            const scoped=new CustomElementRegistry();scoped.initialize(detached);
            check(detached.customElementRegistry===scoped,'initialized Document registry');
            const element=detached.createElementNS('http://www.w3.org/1999/xhtml','x-inner-case');
            class Inner extends HTMLElement {constructor(){super();this.internals=this.attachInternals()}}
            scoped.define('x-inner-case',Inner);
            scoped.upgrade(element);check(element instanceof Inner && element.ownerDocument===detached,'scoped detached Document upgrade preserves its node document');
            check(element.customElementRegistry===scoped,'fixed scoped creation association');
            window.addEventListener('error',event=>event.preventDefault());
            const wrongDocument=detached.createElementNS('http://www.w3.org/1999/xhtml','x-inner-case');
            check(wrongDocument instanceof HTMLUnknownElement && wrongDocument.ownerDocument===detached,'fresh result from current global Document rejects mismatched creation document');
            unsupported(()=>element.attachInternals());unsupported(()=>document.createElement('div').attachInternals());
            class Disabled extends HTMLElement {static disabledFeatures=['internals']}
            scoped.define('x-disabled-internals',Disabled);unsupported(()=>detached.createElementNS('http://www.w3.org/1999/xhtml','x-disabled-internals').attachInternals());
            const closed=element.attachShadow({mode:'closed'});check(element.internals.shadowRoot===closed,'internals sees available closed root');
            check(element.shadowRoot===null,'ordinary closed root remains hidden');
            class Frozen extends HTMLElement {constructor(){super();Object.preventExtensions(this);this.attachInternals()}}
            const frozen=detached.createElementNS('http://www.w3.org/1999/xhtml','x-frozen-internals');
            scoped.define('x-frozen-internals',Frozen);scoped.upgrade(frozen);check(frozen instanceof Frozen,'internal slot ignores property extensibility during upgrade');
            class Host extends HTMLElement {constructor(){super();this.internals=this.attachInternals()}}
            customElements.define('main-host-case',Host);
            const host=document.createElement('main-host-case');
            host.setHTMLUnsafe('<template shadowrootmode="closed" shadowrootclonable shadowrootcustomelementregistry><x-inner-case></x-inner-case></template>');
            const shadow=host.internals.shadowRoot;check(shadow.customElementRegistry===null,'declarative null shadow');
            const child=shadow.firstChild;check(child.customElementRegistry===null,'declarative null child');
            scoped.initialize(shadow);check(shadow.customElementRegistry===scoped&&child instanceof Inner,'initialize shadow and upgrade in scope');
            customElements.initialize(shadow);check(child.customElementRegistry===scoped,'initialize preserves nonnull association');
            document.body.append(element);check(element.customElementRegistry===scoped&&element.internals.shadowRoot===closed,'adoption preserves scoped definition and available root');
            return true;
        })()"#),Value::Bool(true)));
    }

    #[test]
    fn specification_registry_birth_null_shadow_initialize_and_template_import_share_associations() {
        let mut engine=Engine::new();
        let realm=crate::install_live_html(engine.ctx(),r#"<script>
            globalThis.globalCalls=0;
            customElements.define('x-null-birth',class extends HTMLElement{constructor(){super();globalCalls++}});
        </script><div id=host><template shadowrootmode=open shadowrootclonable shadowrootcustomelementregistry>
            <span id=ordinary></span><x-null-birth></x-null-birth>
            <div id=nested><template shadowrootmode=open shadowrootclonable shadowrootcustomelementregistry><x-null-birth></x-null-birth></template></div>
        </template></div><script>
            const check=(value,label)=>{if(!value)throw Error(label)};
            globalThis.scoped=new CustomElementRegistry();
            globalThis.scopedCalls=0;globalThis.constructorRegistry=true;
            scoped.define('x-null-birth',class extends HTMLElement{constructor(){super();scopedCalls++;constructorRegistry&&=this.customElementRegistry===scoped}});
            const host=document.getElementById('host'),root=host.shadowRoot;
            check(root.customElementRegistry===null && root.querySelector('span').customElementRegistry===null && root.querySelector('x-null-birth').customElementRegistry===null,'live parser birth retains null');
            check(globalCalls===0,'global define does not upgrade null-registry elements');
            customElements.upgrade(root);check(globalCalls===0,'upgrade preserves null association');
            const copy=host.cloneNode(true),copyRoot=copy.shadowRoot;
            check(copyRoot.customElementRegistry===null && copyRoot.querySelector('x-null-birth').customElementRegistry===null,'clone retains null shadow associations');
            const imported=document.importNode(host,{deep:true,customElementRegistry:scoped});
            check(imported.shadowRoot.querySelector('x-null-birth').customElementRegistry===null,'import fallback stops at cloned shadow boundary');
            scoped.initialize(copyRoot);
            check(scopedCalls===1 && constructorRegistry && copyRoot.querySelector('span').customElementRegistry===scoped,'initialize upgrades ordinary descendants in own scope');
            const nested=copyRoot.querySelector('#nested').shadowRoot;
            check(nested.customElementRegistry===null && nested.querySelector('x-null-birth').customElementRegistry===null,'initialize stops at nested shadow root');
            customElements.initialize(copyRoot);check(copyRoot.querySelector('x-null-birth').customElementRegistry===scoped,'nonnull associations fixed');
            root.innerHTML='<div><x-null-birth></x-null-birth></div>';
            check(root.firstChild.customElementRegistry===null && root.querySelector('x-null-birth').customElementRegistry===null && globalCalls===0,'fragment birth inherits null');
            const template=document.createElement('template',{customElementRegistry:scoped});template.innerHTML='<x-null-birth></x-null-birth>';
            check(template.content.firstChild.customElementRegistry===null,'template contents remain inert');
            const fragment=document.importNode(template.content,{deep:true,customElementRegistry:scoped});
            check(fragment.firstChild.customElementRegistry===scoped && scopedCalls===2 && constructorRegistry,'import template contents applies fallback outside shadow cloning');
            globalThis.registryBirthPassed=true;
        </script>"#,1024).unwrap();
        while let Some(script)=realm.next_document_parser_script(engine.ctx()).expect("registry parser feed") {
            realm.execute_document_parser_script(engine.ctx(),script.node).expect("registry parser script");
        }
        assert!(matches!(eval(&mut engine,"registryBirthPassed"),Value::Bool(true)));
    }

    #[test]
    fn registry_owned_callbacks_release_retired_realms_but_retained_registries_stay_live() {
        let mut engine = Engine::new();
        let _parent = crate::install(engine.ctx(), "<main></main>", 128).unwrap();
        let child = engine.ctx().create_host_realm();
        let (weak_document, weak_global, weak_hub, registry, retired) = engine.ctx().with_host_realm(&child, |ctx| {
            let realm = crate::install(ctx, "<main></main>", 128).unwrap();
            let weak_document = Rc::downgrade(&realm);
            let global = ctx.global_object();
            let weak_global=ctx.weak_value(&global).expect("realm global identity");
            let weak_hub=realm.custom_element_hub.borrow().clone();
            assert!(weak_hub.upgrade().is_some(),"installed shared registry hub");
            let registry = ctx.eval_in_realm(&global, r#"
                class RetiredElement extends HTMLElement {
                    connectedCallback() { return customElements.get('x-retired-owner'); }
                }
                customElements.define('x-retired-owner', RetiredElement);
                customElements.whenDefined('x-future-owner').then(() => document.body);
                customElements
            "#).ok().expect("registry callbacks and waiter");
            let retired=realm.retire_browsing_context_group(ctx);
            (weak_document, weak_global, weak_hub, registry, retired)
        }).expect("child registry realm");
        for handle in retired {engine.ctx().dispose_host_realm(&handle).expect("dispose retired browser realm");}
        drop(child);
        engine.collect_garbage();
        assert!(weak_document.upgrade().is_some(), "author-retained registry retains its document");
        let get = engine.ctx().member_get(&registry, "get").ok().expect("registry get method");
        let get = JsFunction::from_value(get).expect("callable registry get");
        let constructor = get.call(engine.ctx(), registry.clone(), &[Value::from_string("x-retired-owner".into())])
            .ok().expect("retained registry get");
        drop(get);
        assert!(JsFunction::from_value(constructor.clone()).is_some(), "definition survives realm retirement");
        drop(constructor);
        drop(registry);
        engine.collect_garbage();
        engine.collect_garbage();
        assert!(weak_global.upgrade().is_none(), "released registry leaves no external root for the retired global");
        assert!(weak_hub.upgrade().is_none(), "released JavaScript registry retires its actual native hub");
        assert!(weak_document.upgrade().is_none(), "unreachable definitions, waiter and delivery do not pin the document");
    }

    #[test]
    fn scoped_registries_preserve_creation_clone_import_initialize_and_gc_identity() {
        let mut engine=Engine::new();let _realm=crate::install(engine.ctx(),"<main></main>",256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(v,m)=>{if(!v)throw Error(m)};
            globalThis.scopeA=new CustomElementRegistry();globalThis.scopeB=new CustomElementRegistry();
            class A extends HTMLElement{};class B extends HTMLElement{};class G extends HTMLElement{};
            customElements.define('x-scope-case',G);scopeA.define('x-scope-case',A);scopeB.define('x-scope-case',B);
            globalThis.scopedNode=document.createElement('x-scope-case',{customElementRegistry:scopeA});
            check(scopedNode instanceof A&&scopedNode.customElementRegistry===scopeA,'scoped creation');
            check(document.createElement('x-scope-case') instanceof G,'global creation');
            check(scopedNode.cloneNode() instanceof A,'clone preserves source scoped definition');
            check(document.importNode(scopedNode,{customElementRegistry:scopeB}) instanceof A,'nonnull source registry wins fallback');
            const inactive=document.implementation.createHTMLDocument();
            const candidate=inactive.createElement('x-scope-case');check(candidate.customElementRegistry===null,'inactive document initial registry');
            check(document.importNode(candidate,{customElementRegistry:scopeB}) instanceof B,'null source uses scoped fallback');
            const host=document.createElement('div'),shadow=host.attachShadow({mode:'open',clonable:true,customElementRegistry:null});
            shadow.innerHTML='<x-scope-case></x-scope-case>';const child=shadow.firstChild;
            check(child.customElementRegistry===null&&!(child instanceof G),'null shadow parsing');
            const shadowCopy=document.importNode(host,{deep:true,customElementRegistry:scopeB}).shadowRoot;
            check(shadowCopy.customElementRegistry===null&&shadowCopy.firstChild.customElementRegistry===null,'import fallback stops at cloned null shadow');
            scopeA.initialize(shadow);check(shadow.customElementRegistry===scopeA&&child instanceof A,'initialize null shadow and descendants');
            scopeB.initialize(shadow);check(shadow.customElementRegistry===scopeA&&child.customElementRegistry===scopeA,'initialized association immutable');
            const container=document.createElement('div',{customElementRegistry:scopeB});container.innerHTML='<x-scope-case></x-scope-case>';
            check(container.firstChild instanceof B,'scoped fragment parsing');
            let rejected=false;try{scopeA.define('x-scope-button',class extends HTMLButtonElement{},{extends:'button'})}catch(error){rejected=error.name==='NotSupportedError'}
            check(rejected,'scoped customized built-in rejection');
            rejected=false;try{document.importNode(candidate,{customElementRegistry:null})}catch(error){rejected=error instanceof TypeError}
            check(rejected,'import registry nonnullable conversion');
            globalThis.savedScope=scopeA;return true;
        })()"#),Value::Bool(true)));
        engine.collect_garbage();
        assert!(matches!(eval(&mut engine,"scopedNode.customElementRegistry===savedScope&&scopedNode.cloneNode().customElementRegistry===savedScope"),Value::Bool(true)));
    }

    #[test]
    fn scoped_registry_adoption_keeps_definition_and_effective_global_rules() {
        let mut engine=Engine::new();let _realm=crate::install(engine.ctx(),"<main></main>",256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(v,m)=>{if(!v)throw Error(m)},scoped=new CustomElementRegistry();
            class S extends HTMLElement {adoptedCallback(oldDocument,newDocument){this.movedTo=newDocument}}
            scoped.define('x-scope-adopt',S);
            const node=document.createElement('x-scope-adopt',{customElementRegistry:scoped}),global=document.createElement('div');
            const inactive=document.implementation.createHTMLDocument();inactive.adoptNode(node);
            check(node.customElementRegistry===scoped&&node instanceof S&&node.movedTo===inactive,'scoped adoption definition and callback');
            inactive.adoptNode(global);check(global.customElementRegistry===null,'global adoption into null document');
            document.adoptNode(global);check(global.customElementRegistry===customElements,'global adoption back into active document');
            const nullShadow=document.createElement('div').attachShadow({mode:'open',customElementRegistry:null});
            inactive.adoptNode(nullShadow.host);check(nullShadow.customElementRegistry===null,'explicit null shadow follows effective null global');
            document.adoptNode(nullShadow.host);check(nullShadow.customElementRegistry===customElements,'nondeclarative null shadow follows new effective global');
            const created=inactive.createElement('x-scope-adopt');inactive.body.append(created);scoped.initialize(inactive);scoped.upgrade(created);
            check(created instanceof S&&created.customElementRegistry===scoped&&created.ownerDocument===inactive,'initialized document upgrade preserves requested document');
            window.addEventListener('error',event=>event.preventDefault());
            const failed=inactive.createElement('x-scope-adopt');
            check(failed instanceof HTMLUnknownElement&&failed.ownerDocument===inactive&&failed.customElementRegistry===scoped,'fresh wrong-document result uses failed fallback with fixed registry');
            const copy=inactive.cloneNode(true);check(copy.customElementRegistry===scoped && copy.querySelector('x-scope-adopt') instanceof S,'document clone retains scoped registry and upgrades existing descendants');
            check(document.customElementRegistry===customElements,'DocumentOrShadowRoot getter exposes the actual global registry');return true;
        })()"#),Value::Bool(true)));
    }
    #[lumen_bind::op(hint(js(ce_reactions)))]
    fn mutate_then_throw(_ctx: &mut Ctx, node: &DomNode, failure: Value) -> OpResult<()> {
        node.set_attribute_core("value", "throw-step")?;
        Err(OpError::thrown(failure))
    }

    #[test]
    fn specification_ce_reactions_scopes_are_synchronous_nested_and_preserve_abrupt_identity() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body></body>", 256).unwrap();
        let global = engine.ctx().global_object();
        let operation = engine.ctx().op_function::<mutate_then_throw::Op>();
        engine.ctx().member_set(&global, "mutateThenThrow", operation).ok().expect("install scoped test operation");
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            const log = [];
            class Scoped extends HTMLElement {
                static observedAttributes = ['value','class','data-test','style'];
                attributeChangedCallback(name, before, value) {
                    log.push(this.label + ':' + name + ':' + value + ':start');
                    if (value === 'first') {b.setAttribute('value','nested'); this.setAttribute('value','second');}
                    if (value === 'throw-step') detached.adoptNode(this);
                    log.push(this.label + ':' + name + ':' + value + ':end');
                }
            }
            customElements.define('x-scope-boundary', Scoped);
            const a = new Scoped(), b = new Scoped(); a.label='A'; b.label='B';
            const detached = new DOMParser().parseFromString('<body></body>','text/html');
            a.setAttribute('value','first');
            check(log.join(',') === 'A:value:first:start,B:value:nested:start,B:value:nested:end,A:value:second:start,A:value:second:end,A:value:first:end', 'nested reactions must precede the suspended callback');
            log.length=0;
            a.setAttribute('value', {toString() {
                log.push('convert-start'); b.setAttribute('value','conversion'); log.push('convert-end'); return 'converted';
            }});
            check(log.join(',') === 'convert-start,B:value:conversion:start,B:value:conversion:end,convert-end,A:value:converted:start,A:value:converted:end', 'argument conversion must precede outer reaction scope');
            let conversions=0;
            try {Element.prototype.setAttribute.call({}, 'value', {toString(){conversions++;return 'bad'}});} catch(error) {check(error instanceof TypeError, 'receiver branding');}
            check(conversions===0, 'invalid receiver must fail before conversion');
            a.classList.add('one'); a.dataset.test='two'; a.style.setProperty('color','red');
            check(log.slice(-6).join(',') === 'A:class:one:start,A:class:one:end,A:data-test:two:start,A:data-test:two:end,A:style:color: red;:start,A:style:color: red;:end', 'typed token/dataset/CSS operations deliver synchronously');
            const marker={}; let caught;
            try {mutateThenThrow(a, marker);} catch(error) {caught=error;}
            check(caught===marker && a.ownerDocument===detached && log.slice(-2).join(',') === 'A:value:throw-step:start,A:value:throw-step:end', 'release native arguments before callbacks and preserve original abrupt identity');
            b.setAttribute('value','after-error');
            check(log.slice(-1)[0] === 'B:value:after-error:end', 'scope must unwind after abrupt operation');
            return true;
        })()"#), Value::Bool(true)));
        let agent = agent_reactions(engine.ctx());
        assert!(agent.borrow().stack.is_empty());
        assert_eq!(agent.borrow().invoking, 0);
    }

    #[test]
    fn specification_ce_reactions_upgrade_order_and_foreign_superclass_use_active_registry() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body><x-foreign-super value='initial'></x-foreign-super></body>", 256).unwrap();
        let agent = agent_reactions(engine.ctx());
        let foreign = engine.ctx().create_host_realm();
        let (_foreign_realm, constructor) = match engine.ctx().with_host_realm(&foreign, |ctx| {
            let realm = crate::install(ctx, "", 64).unwrap();
            assert!(Rc::ptr_eq(&agent, &agent_reactions(ctx)));
            let global = ctx.global_object();
            let constructor = ctx.eval_in_realm(&global, r#"
                globalThis.foreignUpgradeLog=[];
                class ForeignSuper extends HTMLElement {
                    static observedAttributes=['value'];
                    constructor(){super();foreignUpgradeLog.push('constructor-start');this.setAttribute('value','changed');foreignUpgradeLog.push('constructor-end')}
                    attributeChangedCallback(name,before,value){foreignUpgradeLog.push('attribute:'+value)}
                    connectedCallback(){foreignUpgradeLog.push('connected')}
                }
                ForeignSuper
            "#).ok().expect("foreign subclass definition");
            (realm, constructor)
        }) {Ok(value)=>value, Err(_)=>panic!("enter foreign reaction realm")};
        let global = engine.ctx().global_object();
        engine.ctx().member_set(&global, "ForeignSuper", constructor).ok().expect("expose foreign superclass");
        assert!(matches!(eval(engine, r#"
            var foreignCandidate=document.querySelector('x-foreign-super');
            customElements.define('x-foreign-super',ForeignSuper);
            if (!(foreignCandidate instanceof ForeignSuper) || foreignCandidate.customElementRegistry!==customElements || foreignCandidate.getAttribute('value')!=='changed')
                throw new Error('upgrade constructor must use active owning registry across realms');
            true
        "#), Value::Bool(true)));
        let log = engine.ctx().with_host_realm(&foreign, |ctx| {
            let global=ctx.global_object();
            ctx.eval_in_realm(&global, "foreignUpgradeLog.join(',')")
        }).ok().expect("foreign reaction log").ok().expect("read foreign reaction log");
        assert!(matches!(log, Value::Str(value) if value.as_str()=="constructor-start,attribute:initial,connected,attribute:changed,constructor-end"));
        assert!(agent_reactions(engine.ctx()).borrow().active_constructors.is_empty());
    }

    #[test]
    fn specification_ce_reactions_aggregate_admission_unwind_and_cancelled_backup_are_bounded() {
        let weak_agent;
        {
            let mut engine = Engine::new();
            let realm = crate::install(engine.ctx(), "", 32).unwrap();
            let agent = agent_reactions(engine.ctx());
            weak_agent = Rc::downgrade(&agent);
            for _ in 0..MAX_REACTION_SCOPES {agent.borrow_mut().push_scope().unwrap();}
            assert!(agent.borrow_mut().push_scope().is_err());
            for _ in 0..MAX_REACTION_SCOPES {abort_reaction_scope(engine.ctx());}
            assert!(agent.borrow().stack.is_empty());
            let hub = hub_from_ctx(engine.ctx()).unwrap();
            let key = RealmNode::new(&realm, realm.session.borrow().document().root());
            agent.borrow_mut().push_scope().unwrap();
            for _ in 0..MAX_REACTIONS / 2 {hub.state.borrow_mut().reactions.push_back(Reaction::Connected(key, Value::Undefined));}
            agent.borrow_mut().push_scope().unwrap();
            for _ in 0..MAX_REACTIONS / 2 {hub.state.borrow_mut().reactions.push_back(Reaction::Connected(key, Value::Undefined));}
            hub.state.borrow_mut().reactions.push_back(Reaction::Connected(key, Value::Undefined));
            assert_eq!(agent.borrow().entries, MAX_REACTIONS);
            assert_eq!(agent.borrow().reactions, MAX_REACTIONS);
            assert!(hub.state.borrow().reactions.allocation_failed);
            end_reaction_scope(engine.ctx());
            end_reaction_scope(engine.ctx());
            assert_eq!(agent.borrow().entries, 0);
            assert_eq!(agent.borrow().reactions, 0);
            assert!(hub.state.borrow().reactions.queues.capacity() <= 256);
            assert!(realm.retained_nodes.borrow().is_empty());
            // Leave a genuinely scheduled backup job pending at interpreter
            // teardown. Weak agent ownership prevents a Hub/agent/Rc cycle.
            hub.state.borrow_mut().reactions.push_back(Reaction::Connected(key, Value::Undefined));
            hub.schedule();
            assert_eq!(agent.borrow().backup.len(), 1);
        }
        assert!(weak_agent.upgrade().is_none());
    }

    #[test]
    fn specification_document_body_setter_and_range_constructor_share_native_mutations() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body><p>old</p></body>", 128).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message)};
            const initialRange=new Range(); check(initialRange.startContainer===document && initialRange.endContainer===document && initialRange.startOffset===0,'associated Range document');
            const original=document.body;
            const failure=(value,name)=>{let caught;try{document.body=value}catch(error){caught=error}
                check(name==='TypeError'?caught instanceof TypeError:caught instanceof DOMException&&caught.name===name,'body exception '+name);
                check(document.body===original,'body failure atomicity')};
            failure('body','TypeError'); failure(document.createElement('div'),'HierarchyRequestError'); failure(null,'HierarchyRequestError');
            const detached=new DOMParser().parseFromString('<html><body><b>new</b></body></html>','text/html');
            const replacement=detached.body;
            const observer=new MutationObserver(()=>{});observer.observe(document.documentElement,{childList:true});
            document.body=replacement;
            check(document.body===replacement && replacement.ownerDocument===document && detached.body===null && original.parentNode===null,'body replacement adopts original wrapper');
            const records=observer.takeRecords();
            check(records.length===1&&records[0].addedNodes.length===1&&records[0].addedNodes[0]===replacement&&records[0].removedNodes[0]===original,'body replacement shared mutation record');observer.disconnect();
            const nonhtml=document.implementation.createDocument('http://www.w3.org/1999/xhtml','test',null);
            const nonhtmlBody=document.createElement('body');nonhtml.body=nonhtmlBody;
            check(nonhtml.documentElement.firstChild===nonhtmlBody&&nonhtml.body===null,'setter accepts any HTML namespace root while getter requires html');
            const fragment=document.createDocumentFragment(); const first=document.createElement('i'),second=document.createElement('u');fragment.append(first,second);
            const holder=document.createElement('div'),before=document.createElement('span'),old=document.createElement('b'),after=document.createElement('em');holder.append(before,old,after);
            const range=new Range();range.selectNode(old);const inside=new Range();inside.selectNodeContents(old);
            observer.observe(holder,{childList:true});observer.observe(fragment,{childList:true});holder.replaceChild(fragment,old);
            const replacementRecords=observer.takeRecords();
            check(replacementRecords.length===2&&replacementRecords[0].target===fragment&&replacementRecords[0].removedNodes.length===2,'fragment emptying observer record');
            check(replacementRecords[1].target===holder&&replacementRecords[1].addedNodes.length===2&&replacementRecords[1].removedNodes[0]===old&&replacementRecords[1].previousSibling===before&&replacementRecords[1].nextSibling===after,'combined replacement records retain sibling identities');
            check(inside.startContainer===holder&&inside.startOffset===1&&range.startOffset===1&&range.endOffset===1,'replacement retains per-step live Range adjustments');
            range.selectNode(first);check(range.intersectsNode(first)&&!range.intersectsNode(second)&&range.intersectsNode(holder)&&!range.intersectsNode(document.createElement('div')),'intersectsNode uses strict boundary and actual roots');
            observer.disconnect();
            const xml=new DOMParser().parseFromString('<root/>','application/xml');
            let error;try{xml.body=document.createElement('body')}catch(value){error=value}
            check(error instanceof DOMException&&error.name==='HierarchyRequestError'&&xml.documentElement.localName==='root','missing HTML root fails');
            return true;
        })()"#), Value::Bool(true)));
    }

    #[test]
    fn specification_parser_adoption_publishes_fixed_state_before_connected_reactions() {
        let mut engine=Engine::new();
        let source=crate::install(engine.ctx(),"<body></body>",128).unwrap();
        assert!(matches!(eval(&mut engine,r#"
            globalThis.log=[];globalThis.originalDocument=document;
            customElements.define('x-published',class extends HTMLElement {
                connectedCallback(){log.push('connected');}
                disconnectedCallback(){log.push('disconnected');}
                adoptedCallback(oldDocument,newDocument){
                    if(oldDocument!==originalDocument||newDocument!==destination)throw new Error('exact adopted documents');
                    log.push('adopted');
                }
            });
            globalThis.moving=document.createElement('x-published');document.body.append(moving);
            globalThis.destination=new DOMParser().parseFromString('<body></body>','text/html');
            log.length=0;true
        "#),Value::Bool(true)));
        let value=eval(&mut engine,"moving");
        let old=engine.ctx().with_instance::<DomNode,_>(&value,|node|node.id).unwrap();
        let destination=eval(&mut engine,"destination");
        let target=engine.ctx().with_instance::<DomDocument,_>(&destination,|document|document.realm.clone()).unwrap();
        let hub=source.custom_element_hub.borrow().upgrade().unwrap();
        prepare_adoption_publication(&source,&target).unwrap();
        begin_reaction_scope(engine.ctx()).ok().expect("adoption reaction scope");
        let mapping={
            let mut source_session=source.session.borrow_mut();let mut target_session=target.session.borrow_mut();
            let source_document=source_session.document_mut();let target_document=target_session.document_mut();
            let old_owner=source_document.node_document(old).unwrap();
            let (new,mapping)=target_document.adopt_subtree_from(source_document,old).unwrap();
            let new_owner=target_document.node_document(new).unwrap();
            publish_adopted_nodes(&source,&target,source_document,target_document,&mapping,&[(old,old_owner,new_owner)]).unwrap();
            assert!(hub.state.borrow().upgraded.contains_key(&RealmNode::new(&target,new)),"fixed custom state is visible before insertion");
            let body=selector::query_selector(target_document,target_document.root(),"body").unwrap().unwrap();
            target_document.append(body,new).unwrap();
            let state=hub.state.borrow();let queue=state.reactions.queues.get(&RealmNode::new(&target,new)).unwrap();
            assert!(queue.iter().position(|reaction|matches!(reaction,Reaction::AdoptedPending(..))).unwrap()
                <queue.iter().position(|reaction|matches!(reaction,Reaction::Connected(..))).unwrap(),"adoption precedes the actual insertion observer reaction");
            mapping
        };
        source.migrate_adopted_state(engine.ctx(),&target,&mapping).unwrap();
        complete_adopted_nodes(engine.ctx(),&target).unwrap();
        end_reaction_scope(engine.ctx());
        assert!(matches!(eval(&mut engine,"log.join(',')==='disconnected,adopted,connected' && moving.ownerDocument===destination && moving.customElementRegistry===null"),Value::Bool(true)));
    }

    #[test]
    fn specification_adoption_migrates_each_owning_registry_and_rendering_fixes_moved_focus() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<body><main></main><section inert></section></body>", 256).unwrap();
        let global = engine.ctx().global_object();
        let log = eval(engine, "globalThis.adoptionLog=[];adoptionLog");
        let foreign = engine.ctx().create_host_realm();
        let (foreign_node, foreign_registry, _foreign_realm) = engine.ctx().with_host_realm(&foreign, |ctx| {
            let realm = crate::install(ctx, "<body><x-foreign-adoption></x-foreign-adoption></body>", 128).unwrap();
            let global = ctx.global_object();
            ctx.member_set(&global, "adoptionLog", log.clone()).ok().expect("share callback log");
            ctx.eval_in_realm(&global, r#"
                customElements.define('x-foreign-adoption',class extends HTMLElement {
                    connectedCallback(){adoptionLog.push('foreign-connected')}
                    disconnectedCallback(){adoptionLog.push('foreign-disconnected')}
                    adoptedCallback(){adoptionLog.push('foreign-adopted')}
                });
            "#).ok().expect("foreign definition");
            (ctx.eval_in_realm(&global,"document.querySelector('x-foreign-adoption')").ok().expect("foreign node"),
             ctx.member_get(&global,"customElements").ok().expect("foreign registry"),realm)
        }).ok().expect("enter foreign document");
        engine.ctx().member_set(&global,"foreignNode",foreign_node).ok().expect("expose foreign node");
        engine.ctx().member_set(&global,"foreignRegistry",foreign_registry).ok().expect("expose foreign registry");
        assert!(matches!(eval(engine,r#"(() => {
            const check=(condition,message)=>{if(!condition)throw new Error(message)};
            customElements.define('x-main-adoption',class extends HTMLElement {
                connectedCallback(){adoptionLog.push('main-connected')}
                disconnectedCallback(){adoptionLog.push('main-disconnected')}
                adoptedCallback(){adoptionLog.push('main-adopted')}
            });
            const container=document.querySelector('main'); const own=document.createElement('x-main-adoption');container.appendChild(own);container.appendChild(foreignNode);
            check(foreignNode.customElementRegistry===customElements,'global registry follows first adoption');
            adoptionLog.length=0;
            const detached=new DOMParser().parseFromString('<html><body></body></html>','text/html');detached.body.appendChild(container);
            check(adoptionLog.join(',')==='main-disconnected,main-adopted,main-connected,foreign-disconnected,foreign-adopted,foreign-connected','mixed owning registries drain each element migration reaction queue');
            check(own.customElementRegistry===null&&foreignNode.customElementRegistry===null,'global registries follow parser document adoption');
            globalThis.focusControl=document.createElement('input');document.body.append(focusControl);focusControl.focus();
            const events=[];document.addEventListener('focusout',event=>events.push(event.target));globalThis.focusEvents=events;
            document.querySelector('section').moveBefore(focusControl,null);
            check(document.activeElement===focusControl&&events.length===0,'move retains focus before rendering update');
            return true;
        })()"#), Value::Bool(true)));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(matches!(eval(engine,"document.activeElement!==focusControl&&focusEvents.length===1&&focusEvents[0]===focusControl"),Value::Bool(true)));
        assert!(matches!(eval(engine,r#"(() => {
            document.body.append(focusControl);focusControl.focus();
            const holder=document.createElement('div');document.body.append(holder);holder.style.display='none';holder.moveBefore(focusControl,null);
            return document.activeElement===focusControl&&focusEvents.length===1;
        })()"#),Value::Bool(true)));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(matches!(eval(engine,"document.activeElement!==focusControl&&focusEvents.length===2"),Value::Bool(true)));
    }

    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let stack = engine
                    .ctx()
                    .get_member(&error, "stack")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(text) => Some(text.as_str().to_owned()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "JavaScript exception without a stack".into());
                panic!("JavaScript evaluation threw: {stack}\nSource: {source}");
            }
        }
    }

    #[test]
    fn namespaced_attribute_mutations_reach_observers_and_custom_element_reactions() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "<x-ns id='target'></x-ns>", 64).unwrap();
        eval(
            &mut engine,
            r#"
            var element=document.getElementById('target');
            element.setAttributeNS('urn:example','a:value','before');
            var changes=[];
            class NamespaceElement extends HTMLElement {
                static get observedAttributes(){return ['value'];}
                attributeChangedCallback(name,oldValue,newValue,namespace){
                    changes.push([name,oldValue,newValue,namespace]);
                }
            }
            customElements.define('x-ns',NamespaceElement);
        "#,
        );
        engine.ctx().drain_microtasks_for_host();
        eval(
            &mut engine,
            r#"
            var observer=new MutationObserver(function(){});
            observer.observe(element,{attributes:true,attributeOldValue:true});
            var filtered=new MutationObserver(function(){});
            filtered.observe(element,{attributes:true,attributeOldValue:true,attributeFilter:['value']});
            element.setAttributeNS('urn:example','b:value','after');
            element.removeAttributeNS('urn:example','value');
            element.setAttribute('value','plain');
            var records=observer.takeRecords(), subset=filtered.takeRecords();
            var observerValid=records.length===3 &&
                records[0].attributeName==='value' && records[0].attributeNamespace==='urn:example' && records[0].oldValue==='before' &&
                records[1].attributeName==='value' && records[1].attributeNamespace==='urn:example' && records[1].oldValue==='after' &&
                records[2].attributeNamespace===null && records[2].oldValue===null &&
                subset.length===1 && subset[0].attributeName==='value' && subset[0].attributeNamespace===null;
        "#,
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
            observerValid && changes.length===4 &&
                changes[0][0]==='value' && changes[0][1]===null && changes[0][2]==='before' && changes[0][3]==='urn:example' &&
                changes[1][1]==='before' && changes[1][2]==='after' && changes[1][3]==='urn:example' &&
                changes[2][1]==='after' && changes[2][2]===null && changes[2][3]==='urn:example' &&
                changes[3][1]===null && changes[3][2]==='plain' && changes[3][3]===null
        "#
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn disabled_shadow_features_are_snapshotted_and_share_imperative_declarative_policy() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main></main>", 256).unwrap();
        assert!(matches!(eval(&mut engine, r#"
            var reads=0, features=['shadow'], marker={};
            class NoShadow extends HTMLElement {
              static get disabledFeatures(){reads++;return features}
            }
            var undefinedHost=document.createElement('x-no-shadow');
            undefinedHost.attachShadow({mode:'closed'});
            customElements.define('x-no-shadow',NoShadow);
            features.length=0;
            var denied=false, existingDenied=false;
            try{document.createElement('x-no-shadow').attachShadow({mode:'open'})}catch(e){denied=e.name==='NotSupportedError'}
            try{undefinedHost.attachShadow({mode:'closed'})}catch(e){existingDenied=e.name==='NotSupportedError'}
            class UpperShadow extends HTMLElement {static get disabledFeatures(){return new Set(['SHADOW'])}}
            customElements.define('x-upper-shadow',UpperShadow);
            var upper=document.createElement('x-upper-shadow').attachShadow({mode:'open'});
            var main=document.querySelector('main');
            main.setHTMLUnsafe('<x-no-shadow><template shadowrootmode="open"><b>fallback</b></template></x-no-shadow><x-upper-shadow><template shadowrootmode="closed" shadowrootserializable><i>allowed</i></template></x-upper-shadow>');
            var blocked=main.firstChild, allowed=main.lastChild;
            var thrown=false;
            class BadFeatures extends HTMLElement {static get disabledFeatures(){throw marker}}
            try{customElements.define('x-bad-features',BadFeatures)}catch(e){thrown=e===marker}
            reads===1 && denied && existingDenied && upper instanceof ShadowRoot &&
              blocked instanceof NoShadow && blocked.shadowRoot===null && blocked.firstChild instanceof HTMLTemplateElement &&
              blocked.firstChild.content.textContent==='fallback' &&
              allowed.getHTML({serializableShadowRoots:true})==='<template shadowrootmode="closed" shadowrootserializable=""><i>allowed</i></template>' &&
              thrown && customElements.get('x-bad-features')===undefined
        "#), Value::Bool(true)));
    }

    #[test]
    fn customized_birth_state_survives_content_edits_clones_imports_and_declarative_policy() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main></main>", 512).unwrap();
        assert!(matches!(eval(&mut engine, r#"
            function require(value,label){if(!value)throw new Error(label)}
            class BirthDiv extends HTMLDivElement {}
            var pending=document.createElement('div',{is:'x-birth-div'});
            var nullBirth=document.createElement('p',{is:null});
            var undefinedBirth=document.createElement('p',{is:undefined});
            var nullNamespaceBirth=document.createElementNS('http://www.w3.org/1999/xhtml','p',{is:null});
            require(!nullBirth.hasAttribute('is') && nullBirth.outerHTML==='<p is="null"></p>' &&
              undefinedBirth.outerHTML==='<p></p>' && nullNamespaceBirth.outerHTML==='<p is="null"></p>','birth-domstring-null');
            require(!pending.hasAttribute('is') && pending.outerHTML==='<div is="x-birth-div"></div>','virtual-pending');
            pending.setAttribute('is','changed');
            var clone=pending.cloneNode();
            var ordinary=document.createElement('div');ordinary.setAttribute('is','x-birth-div');
            var main=document.querySelector('main');main.append(pending,clone,ordinary);
            customElements.define('x-birth-div',BirthDiv,{extends:'div'});
            require(pending instanceof BirthDiv && clone instanceof BirthDiv && !(ordinary instanceof BirthDiv),'immutable-upgrade-state');
            require(pending.outerHTML==='<div is="changed"></div>','real-content-precedence');
            pending.removeAttribute('is');pending.setAttribute('class','foo');
            require(pending.outerHTML==='<div is="x-birth-div" class="foo"></div>','virtual-order');
            var holder=document.createElement('main');holder.innerHTML=pending.outerHTML;
            require(holder.firstChild instanceof BirthDiv && holder.firstChild.getAttribute('is')==='x-birth-div','parse-round-trip');
            var namespaced=document.createElementNS('http://www.w3.org/1999/xhtml','h:div',{is:'x-birth-div'});
            require(namespaced instanceof BirthDiv && namespaced.prefix==='h' && namespaced.localName==='div' && !namespaced.hasAttribute('is'),'namespace-options');
            var foreign=document.createElementNS('http://www.w3.org/2000/svg','g',{is:'x-birth-div'});
            require(!(foreign instanceof BirthDiv) && !foreign.hasAttribute('is'),'foreign-birth');
            var detached=new DOMParser().parseFromString('<main></main>','text/html');
            var adopted=detached.adoptNode(pending);
            require(adopted===pending && adopted instanceof BirthDiv && !adopted.hasAttribute('is'),'adoption');
            var imported=document.importNode(adopted,true);
            require(imported instanceof BirthDiv && imported.outerHTML==='<div is="x-birth-div" class="foo"></div>','import-state');
            var definedClone=imported.cloneNode();
            require(definedClone instanceof BirthDiv && definedClone!==imported && definedClone.outerHTML===imported.outerHTML,'defined-clone-state');
            var branch=detached.createElement('section');branch.appendChild(adopted);
            var deepImport=document.importNode(branch,true), shallowImport=document.importNode(branch,false);
            require(deepImport.firstChild instanceof BirthDiv && deepImport.firstChild.ownerDocument===document &&
              deepImport.firstChild.outerHTML===imported.outerHTML && !shallowImport.firstChild,'deep-import-state');
            class AutonomousBirth extends HTMLElement {static get disabledFeatures(){return ['shadow']}}
            customElements.define('x-autonomous-birth',AutonomousBirth);
            var autonomous=document.createElement('x-autonomous-birth',{is:'unrelated-name'});
            var autonomousDenied=false;try{autonomous.attachShadow({mode:'open'})}catch(e){autonomousDenied=e.name==='NotSupportedError'}
            require(autonomous instanceof AutonomousBirth && autonomousDenied,'autonomous-lookup-precedence');
            class DisabledHeading extends HTMLHeadingElement {static get disabledFeatures(){return ['shadow']}}
            var heading=document.createElement('h2',{is:'x-disabled-birth'});
            heading.setAttribute('is','misleading');
            main.appendChild(heading);
            customElements.define('x-disabled-birth',DisabledHeading,{extends:'h2'});
            var denied=false;try{heading.attachShadow({mode:'open'})}catch(e){denied=e.name==='NotSupportedError'}
            require(heading instanceof DisabledHeading && denied,'disabled-birth-policy');
            main.setHTMLUnsafe('<h2 is="x-disabled-birth"><template shadowrootmode="open"><b>fallback</b></template></h2>');
            require(main.firstChild instanceof DisabledHeading && main.firstChild.shadowRoot===null && main.firstChild.firstChild.content.textContent==='fallback','parser-birth-policy');
            var templateConstructions=0;
            class TemplateBirth extends HTMLElement {constructor(){super();templateConstructions++}}
            customElements.define('x-template-birth',TemplateBirth);
            var template=document.createElement('template');
            template.innerHTML='<x-template-birth></x-template-birth>';
            customElements.upgrade(template);
            require(templateConstructions===0 && !(template.content.firstChild instanceof TemplateBirth),'template-target-inert');
            main.innerHTML='<template><x-template-birth></x-template-birth></template>';
            require(templateConstructions===0,'nested-template-inert');
            main.appendChild(template.content.firstChild);
            require(templateConstructions===1 && main.lastChild instanceof TemplateBirth,'template-content-insertion');
            var throwConstructions=0;
            class MarkupThrower extends HTMLElement {constructor(){super();throwConstructions++;throw new Error('expected-markup-constructor')}}
            customElements.define('x-markup-thrower',MarkupThrower);
            main.innerHTML='<section><x-markup-thrower></x-markup-thrower><x-template-birth></x-template-birth></section>';
            require(throwConstructions===1 && templateConstructions===2 && main.firstChild.lastChild instanceof TemplateBirth,'reported-constructor-continues');
            template.innerHTML='<x-template-birth></x-template-birth>';
            var inertTemplate=template.cloneNode(true), inertFragment=inertTemplate.content, constructionsBefore=templateConstructions;
            require(!(inertFragment.firstChild instanceof TemplateBirth),'cloned-template-fragment-inert');
            main.appendChild(inertFragment);
            require(templateConstructions===constructionsBefore+1 && main.lastChild instanceof TemplateBirth && !inertFragment.firstChild,'fragment-insertion-upgrade');
            for(var operation of ['append','prepend','replaceChildren','replaceChild','before','after','replaceWith']){
              var batchTemplate=template.cloneNode(true), batchFragment=batchTemplate.content;
              var batchHost=document.createElement('main'), placeholder=document.createElement('span');
              batchHost.appendChild(placeholder);main.appendChild(batchHost);
              var beforeBatch=templateConstructions;
              require(!(batchFragment.firstChild instanceof TemplateBirth),'batch-fragment-inert-'+operation);
              if(operation==='replaceChild'){
                require(batchHost.replaceChild(batchFragment,placeholder)===placeholder,'replacement-identity');
              }else if(operation==='before'||operation==='after'||operation==='replaceWith'){
                placeholder[operation](batchFragment);
              }else{batchHost[operation](batchFragment)}
              var inserted=operation==='append'||operation==='after'?batchHost.lastChild:batchHost.firstChild;
              require(templateConstructions===beforeBatch+1 && inserted instanceof TemplateBirth &&
                inserted.ownerDocument===document && !batchFragment.firstChild,'batch-fragment-upgrade-'+operation);
            }
            var insertionVisits=[];
            class OrderedBirth extends HTMLElement {constructor(){super();insertionVisits.push(this.id)}}
            customElements.define('x-ordered-birth',OrderedBirth);
            var orderedTemplate=document.createElement('template');
            orderedTemplate.innerHTML='<x-ordered-birth id="a"></x-ordered-birth><x-ordered-birth id="b"></x-ordered-birth>';
            var repeated=orderedTemplate.content, explicitA=repeated.firstChild;
            main.append(...Array(64).fill(repeated),explicitA,repeated);
            require(insertionVisits.join(',')==='a,b' && main.lastChild.id==='b' && !repeated.firstChild,'repeated-fragment-final-order');
            orderedTemplate.innerHTML='<x-ordered-birth id="c"></x-ordered-birth><x-ordered-birth id="d"></x-ordered-birth>';
            var mixed=orderedTemplate.content, explicitC=mixed.firstChild;
            main.append(mixed,explicitC);
            require(insertionVisits.join(',')==='a,b,d,c' && main.lastChild.id==='c' && !mixed.firstChild,'mixed-fragment-child-final-order');
            var ownerConstructions=0;
            class OwnerBirth extends HTMLElement {constructor(){super();ownerConstructions++}}
            customElements.define('x-owner-birth',OwnerBirth);
            var ownerTemplate=document.createElement('template');
            ownerTemplate.innerHTML='<x-owner-birth><template><b>nested</b></template></x-owner-birth>';
            var inertOwner=ownerTemplate.content.ownerDocument;
            require(inertOwner instanceof Document && inertOwner!==document && inertOwner===template.content.ownerDocument,'shared-inert-document');
            require(inertOwner.ownerDocument===null && inertOwner.defaultView===null && inertOwner.location===null && inertOwner.URL==='about:blank' && inertOwner.contentType==='text/html','inert-document-metadata');
            require(inertOwner.documentElement===null && inertOwner.body===null && inertOwner.head===null && inertOwner.querySelector('main')===null && inertOwner.getElementById('a')===null,'inert-document-tree');
            var inertNode=ownerTemplate.content.firstChild, nestedTemplate=inertNode.firstChild;
            require(inertNode.ownerDocument===inertOwner && nestedTemplate.ownerDocument===inertOwner && nestedTemplate.content.ownerDocument===inertOwner && nestedTemplate.content.firstChild.ownerDocument===inertOwner,'nested-inert-owner');
            var standaloneContent=ownerTemplate.content.cloneNode(true), inertCreated=inertOwner.createElement('x-owner-birth');
            var inertImported=inertOwner.importNode(inertNode,true), inertFragment=inertOwner.createDocumentFragment();
            require(ownerConstructions===0 && standaloneContent.ownerDocument===inertOwner && standaloneContent.firstChild.ownerDocument===inertOwner && inertCreated.ownerDocument===inertOwner && inertImported.ownerDocument===inertOwner && inertFragment.ownerDocument===inertOwner,'inert-clone-create-import');
            require(!(inertCreated instanceof OwnerBirth) && !(inertImported instanceof OwnerBirth),'inert-registry');
            var inertRange=inertOwner.createRange();
            require(inertRange.startContainer===inertOwner && inertRange.endContainer===inertOwner,'inert-range-root');
            inertCreated.setAttribute('state','old');
            var inertAttr=inertCreated.getAttributeNode('state');
            inertCreated.removeAttribute('state');
            require(inertAttr.ownerDocument===inertOwner && inertAttr.ownerElement===null,'inert-attr-detached-owner');
            inertCreated.setAttributeNode(inertAttr);
            var replacementAttr=inertOwner.createAttribute('state');replacementAttr.value='new';
            require(inertCreated.setAttributeNode(replacementAttr)===inertAttr && inertAttr.ownerDocument===inertOwner,'inert-attr-replaced-owner');
            var liveAttrTarget=document.createElement('div');liveAttrTarget.setAttributeNode(inertAttr);
            require(inertAttr.ownerDocument===document,'attr-to-live-owner');
            class InertShadowPolicy extends HTMLElement {static get disabledFeatures(){return ['shadow'];}}
            customElements.define('x-inert-shadow-policy',InertShadowPolicy);
            var inertShadowHost=inertOwner.createElement('x-inert-shadow-policy');
            require(inertShadowHost.attachShadow({mode:'open'}).host===inertShadowHost,'inert-shadow-registry-policy');
            inertNode.remove();require(inertNode.ownerDocument===inertOwner && nestedTemplate.content.ownerDocument===inertOwner,'inert-detachment');
            var activeImport=document.importNode(standaloneContent,true);
            require(ownerConstructions===1 && activeImport.ownerDocument===document && activeImport.firstChild instanceof OwnerBirth && activeImport.firstChild.ownerDocument===document && activeImport.firstChild.firstChild.content.ownerDocument===inertOwner,'import-active-owner');
            main.appendChild(inertCreated);
            require(ownerConstructions===2 && inertCreated instanceof OwnerBirth && inertCreated.ownerDocument===document,'inert-to-active-adoption');
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn standalone_customized_interfaces_share_metadata_brands_and_native_lifecycles() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main><div is='x-existing-div'></div></main>", 512).unwrap();
        assert!(matches!(eval(&mut engine, r#"
            function require(value,label){if(!value)throw new Error(label)}
            var old=document.querySelector('div');
            class ExistingDiv extends HTMLDivElement {constructor(){super();this.upgraded=true}}
            customElements.define('x-existing-div',ExistingDiv,{extends:'div'});
            require(old instanceof ExistingDiv && old.upgraded && document.querySelector('div')===old,'existing-div');
            var data=[['br',HTMLBRElement],['head',HTMLHeadElement],['html',HTMLHtmlElement],
              ['body',HTMLBodyElement],['title',HTMLTitleElement],['base',HTMLBaseElement],
              ['link',HTMLLinkElement],['script',HTMLScriptElement],['output',HTMLOutputElement],
              ['a',HTMLAnchorElement],['area',HTMLAreaElement],['details',HTMLDetailsElement],
              ['slot',HTMLSlotElement],['canvas',HTMLCanvasElement],['img',HTMLImageElement],
              ['audio',HTMLAudioElement],['video',HTMLVideoElement],['address',HTMLElement],['section',HTMLElement]];
            for(var entry of data){
              let tag=entry[0], Base=entry[1];
              let Custom=class extends Base {constructor(){super();this.localSeen=this.localName;this.addEventListener('probe',()=>this.probed=true)}};
              customElements.define('x-native-'+tag,Custom,{extends:tag});
              let direct=new Custom(), created=document.createElement(tag,{is:'x-native-'+tag});
              direct.dispatchEvent(new Event('probe'));
              require(direct instanceof Custom && direct instanceof Base && direct.localSeen===tag && direct.probed,'direct-'+tag);
              require(created instanceof Custom && created.localSeen===tag,'created-'+tag);
              let illegal=false;try{new Base()}catch(e){illegal=e.name==='TypeError'}
              require(illegal,'illegal-'+tag);
              if(tag==='a'){direct.href='https://links.test/path';require(direct.hostname==='links.test','anchor-state')}
              if(tag==='output'){direct.value='value';require(direct.value==='value','output-state')}
              if(tag==='canvas'){let ctx=direct.getContext('2d');ctx.fillStyle='red';ctx.fillRect(0,0,1,1);require(ctx.getImageData(0,0,1,1).data[0]===255,'canvas-native')}
              if(tag==='audio'||tag==='video'){direct.muted=true;direct.playbackRate=1.5;require(direct.muted && direct.playbackRate===1.5 && direct instanceof HTMLMediaElement,'media-state-'+tag)}
            }
            require(!(document.createElement('div','x-existing-div') instanceof ExistingDiv),'ignored-string-overload');
            var invalid=false;try{customElements.define('x-unknown-base',class extends HTMLElement {},{extends:'not-a-built-in'})}catch(e){invalid=e.name==='NotSupportedError'}
            require(invalid,'unknown-base');
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn customized_html_constructor_uses_new_target_realm_and_direct_native_receiver() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main><p is='x-direct-paragraph' id='existing'></p></main>", 256).unwrap();
        let global = engine.ctx().global_object();
        let paragraph = engine.ctx().member_get(&global, "HTMLParagraphElement").ok().expect("parent paragraph constructor");
        let parent_prototype = engine.ctx().member_get(&paragraph, "prototype").ok().expect("parent paragraph prototype");
        let foreign_handle = engine.ctx().create_host_realm();
        let (_foreign_realm, targets, foreign_prototype) = match engine.ctx().with_host_realm(&foreign_handle, |ctx| {
            let realm = super::super::install(ctx, "<main></main>", 128).unwrap();
            let global = ctx.global_object();
            ctx.set_member(&global, "parentParagraphPrototype", parent_prototype).ok().expect("foreign definition prototype");
            let targets = match ctx.eval_in_realm(&global, r#"
                [null,undefined,5,'string'].map(value=>{
                  let enabled=false,gets=0;
                  function ForeignTarget(){}
                  const target=new Proxy(ForeignTarget,{get(object,key,receiver){
                    if(key==='prototype'){gets++;return enabled?value:parentParagraphPrototype}
                    return Reflect.get(object,key,receiver);
                  }});
                  return {target,enable(){enabled=true},reset(){gets=0},get gets(){return gets}};
                })
            "#) { Ok(value) => value, Err(_) => panic!("foreign dynamic constructor fixture") };
            let constructor = ctx.member_get(&global, "HTMLParagraphElement").ok().expect("foreign paragraph constructor");
            let prototype = ctx.member_get(&constructor, "prototype").ok().expect("foreign paragraph prototype");
            (realm, targets, prototype)
        }) { Ok(values) => values, Err(_) => panic!("enter foreign HTML constructor realm") };
        engine.ctx().set_member(&global, "foreignParagraphTargets", targets).ok().expect("install foreign targets");
        engine.ctx().set_member(&global, "foreignParagraphPrototype", foreign_prototype).ok().expect("install foreign paragraph prototype");
        assert!(matches!(eval(&mut engine, r#"
            function require(value,label){if(!value)throw new Error(label)}
            var savedObjectSetter=Object.setPrototypeOf,savedReflectSetter=Reflect.setPrototypeOf;
            Object.setPrototypeOf=Reflect.setPrototypeOf=function(){throw new Error('author-prototype-setter-must-not-run')};
            for(var i=0;i<foreignParagraphTargets.length;i++){
              var record=foreignParagraphTargets[i];
              customElements.define('x-foreign-paragraph-'+i,record.target,{extends:'p'});
              record.enable();record.reset();
              var paragraph=Reflect.construct(HTMLParagraphElement,[],record.target);
              require(record.gets===1,'single-native-prototype-get-'+i);
              require(Object.getPrototypeOf(paragraph)===foreignParagraphPrototype && paragraph.ownerDocument===document && paragraph.localName==='p','foreign-html-fallback-'+i);
              require(paragraph.outerHTML==='<p is="x-foreign-paragraph-'+i+'"></p>','foreign-birth-'+i);
              record.reset();var wrong=false;
              try{Reflect.construct(HTMLDivElement,[],record.target)}catch(e){wrong=e.name==='TypeError'}
              require(wrong && record.gets===0,'superclass-check-before-get-'+i);
            }
            var existing=document.getElementById('existing');
            class DirectParagraph extends HTMLParagraphElement {}
            customElements.define('x-direct-paragraph',DirectParagraph,{extends:'p'});
            var direct=Reflect.construct(HTMLParagraphElement,[],DirectParagraph);
            require(direct instanceof DirectParagraph && direct instanceof HTMLParagraphElement && direct.ownerDocument===document,'direct-native-object-prototype');
            require(document.getElementById('existing')===existing && existing instanceof DirectParagraph,'upgrade-identity');
            var wrongPrototypeGets=0;
            class WrongParagraph extends HTMLParagraphElement {}
            var wrongProxy=new Proxy(WrongParagraph,{get(target,key,receiver){if(key==='prototype')wrongPrototypeGets++;return Reflect.get(target,key,receiver)}});
            customElements.define('x-wrong-paragraph',wrongProxy);wrongPrototypeGets=0;
            var wrongNew=false;try{new wrongProxy()}catch(e){wrongNew=e.name==='TypeError'}
            require(wrongNew && wrongPrototypeGets===0,'derived-superclass-before-prototype-get');
            var wrongReflect=false;try{Reflect.construct(HTMLParagraphElement,[],wrongProxy)}catch(e){wrongReflect=e.name==='TypeError'}
            require(wrongReflect && wrongPrototypeGets===0,'reflect-superclass-before-prototype-get');
            var validPrototypeGets=0;
            class ValidParagraph extends HTMLParagraphElement {}
            var validProxy=new Proxy(ValidParagraph,{get(target,key,receiver){if(key==='prototype')validPrototypeGets++;return Reflect.get(target,key,receiver)}});
            customElements.define('x-valid-paragraph',validProxy,{extends:'p'});validPrototypeGets=0;
            var valid=new validProxy();
            require(valid instanceof ValidParagraph && valid.ownerDocument===document && validPrototypeGets===1,'derived-valid-single-prototype-get');
            Object.setPrototypeOf=savedObjectSetter;Reflect.setPrototypeOf=savedReflectSetter;
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn legacy_image_audio_factories_share_prototypes_and_identity_without_html_constructor_aliases() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main></main>", 256).unwrap();
        let foreign_handle = engine.ctx().create_host_realm();
        let (_foreign_realm, foreign_target, image_prototype, audio_prototype) =
            match engine.ctx().with_host_realm(&foreign_handle, |ctx| {
                let realm = super::super::install(ctx, "<main></main>", 128).unwrap();
                let global = ctx.global_object();
                let target = match ctx.eval_in_realm(&global,
                    "function ForeignFactoryTarget(){}; ForeignFactoryTarget.prototype=null; ForeignFactoryTarget") {
                    Ok(value) => value,
                    Err(_) => panic!("foreign factory target construction"),
                };
                let image = ctx.member_get(&global, "HTMLImageElement").ok().expect("foreign image constructor");
                let audio = ctx.member_get(&global, "HTMLAudioElement").ok().expect("foreign audio constructor");
                let image_prototype = ctx.member_get(&image, "prototype").ok().expect("foreign image prototype");
                let audio_prototype = ctx.member_get(&audio, "prototype").ok().expect("foreign audio prototype");
                (realm, target, image_prototype, audio_prototype)
            }) {
                Ok(values) => values,
                Err(_) => panic!("enter foreign factory realm"),
            };
        let global = engine.ctx().global_object();
        for (name, value) in [("foreignFactoryTarget", foreign_target),
            ("foreignImagePrototype", image_prototype), ("foreignAudioPrototype", audio_prototype)] {
            engine.ctx().set_member(&global, name, value).ok().expect("install foreign factory fixture value");
        }
        assert!(matches!(eval(&mut engine, r#"
            function require(value,label){if(!value)throw new Error(label)}
            require(Image!==HTMLImageElement && Audio!==HTMLAudioElement,'distinct-factories');
            require(Image.prototype===HTMLImageElement.prototype && Audio.prototype===HTMLAudioElement.prototype,'shared-prototypes');
            require(Image.prototype.constructor===HTMLImageElement && Audio.prototype.constructor===HTMLAudioElement,'canonical-prototype-constructor');
            var image=new Image(4,5), called=Image(0,-1), empty=Image();
            require(image instanceof HTMLImageElement && image.width===4 && image.height===5,'image-dimensions');
            require(called.getAttribute('width')==='0' && called.getAttribute('height')==='4294967295' && !empty.hasAttribute('width'),'image-webidl-optional');
            class DerivedImage extends Image {}
            var derived=new DerivedImage(7,8);
            require(derived instanceof DerivedImage && derived instanceof HTMLImageElement && derived.width===7,'factory-new-target');
            derived.id='factory-image';document.querySelector('main').appendChild(derived);
            require(document.getElementById('factory-image')===derived,'factory-identity');
            var audio=new Audio('tone.wav'), audioCalled=Audio(), audioEmpty=Audio('');
            require(audio instanceof HTMLAudioElement && audio instanceof HTMLMediaElement && audio.getAttribute('src')==='tone.wav' && audio.getAttribute('preload')==='auto','audio-source');
            require(!audioCalled.hasAttribute('src') && audioEmpty.getAttribute('src')==='','audio-optional');
            class DerivedAudio extends Audio {}
            var derivedAudio=new DerivedAudio('other.wav');
            require(derivedAudio instanceof DerivedAudio && derivedAudio instanceof HTMLAudioElement,'audio-new-target');
            var relevantDocument=document;globalThis.document=null;
            var relevantImage=Image(), relevantAudio=Audio();globalThis.document=relevantDocument;
            require(relevantImage.ownerDocument===relevantDocument && relevantAudio.ownerDocument===relevantDocument,'factory-relevant-document');
            var foreignImage=Reflect.construct(Image,[],foreignFactoryTarget);
            var foreignAudio=Reflect.construct(Audio,[],foreignFactoryTarget);
            require(Object.getPrototypeOf(foreignImage)===foreignImagePrototype && Object.getPrototypeOf(foreignAudio)===foreignAudioPrototype,'factory-foreign-intrinsic');
            require(foreignImage.ownerDocument===document && foreignAudio.ownerDocument===document,'factory-original-document');
            for(var Base of [HTMLImageElement,HTMLAudioElement,HTMLVideoElement]){
              var illegal=false;try{new Base()}catch(e){illegal=e.name==='TypeError'}
              require(illegal,'html-illegal-constructor');
            }
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn plain_customized_interfaces_share_multitag_constructor_brand_and_shadow_policy() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main><h2 is='x-no-heading' id='old'></h2></main>", 256).unwrap();
        assert!(matches!(eval(&mut engine, r#"
            function require(value,label){if(!value)throw new Error(label)}
            var old=document.getElementById('old');
            class NoHeading extends HTMLHeadingElement {
              static get disabledFeatures(){return ['shadow']}
              constructor(){super();this.constructed=this.localName==='h2'}
            }
            customElements.define('x-no-heading',NoHeading,{extends:'h2'});
            require(old instanceof NoHeading && old.constructed,'upgrade-identity');
            var direct=new NoHeading(), created=document.createElement('h2',{is:'x-no-heading'});
            require(direct instanceof HTMLHeadingElement && direct.localName==='h2' && !direct.hasAttribute('is') && direct.outerHTML==='<h2 is="x-no-heading"></h2>','direct-heading');
            require(created instanceof NoHeading,'created-heading');
            for(var host of [old,direct,created]){
              var denied=false;
              try{host.attachShadow({mode:'open'})}catch(e){denied=e.name==='NotSupportedError'}
              require(denied,'heading-policy');
            }
            class AllowedHeading extends HTMLHeadingElement {static get disabledFeatures(){return ['SHADOW']}}
            customElements.define('x-allowed-heading',AllowedHeading,{extends:'h5'});
            var allowed=new AllowedHeading();
            require(allowed.localName==='h5' && allowed.attachShadow({mode:'open'}) instanceof ShadowRoot,'multitag-heading');
            class Insert extends HTMLModElement {}
            customElements.define('x-insert',Insert,{extends:'ins'});
            var insertion=new Insert();
            require(insertion.localName==='ins' && insertion instanceof HTMLModElement,'multitag-mod');
            class CustomButton extends HTMLButtonElement {constructor(){super();this.addEventListener('ping',()=>this.pinged=true)}}
            customElements.define('x-button',CustomButton,{extends:'button'});
            var button=new CustomButton();button.dispatchEvent(new Event('ping'));
            require(button.localName==='button' && button.type==='submit' && button.pinged,'plain-native-data');
            var illegal=false;try{new HTMLHeadingElement()}catch(e){illegal=e.name==='TypeError'}
            require(illegal,'unregistered-constructor');
            var main=document.querySelector('main');
            main.setHTMLUnsafe('<h2 is="x-no-heading"><template shadowrootmode="open"><b>retained</b></template></h2><h5 is="x-allowed-heading"><template shadowrootmode="open"><i>shadow</i></template></h5>');
            require(main.firstChild instanceof NoHeading && main.firstChild.shadowRoot===null && main.firstChild.firstChild.content.textContent==='retained','declarative-disabled-heading');
            require(main.lastChild instanceof AllowedHeading && main.lastChild.shadowRoot.textContent==='shadow','declarative-allowed-heading');
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn specification_parser_adoption_preflight_deduplicates_hubs_and_observer_targets() {
        let mut engine=Engine::new();let source=super::super::install(engine.ctx(),"<main></main>",128).unwrap();
        let parsed=eval(&mut engine,"new DOMParser().parseFromString('<main></main>','text/html')");
        let target=engine.ctx().with_instance::<DomDocument,_>(&parsed,|document|document.realm.clone()).unwrap();
        prepare_parser_adoption_publication(&[source.clone(),target.clone(),source.clone()]).unwrap();
        let source_count=source.mutation_sinks.borrow().len();let target_count=target.mutation_sinks.borrow().len();
        let hub=source.custom_element_hub.borrow().upgrade().unwrap();
        assert_eq!(hub.attached_realms.borrow().iter().filter_map(std::rc::Weak::upgrade).filter(|realm|Rc::ptr_eq(realm,&target)).count(),1);
        for _ in 0..8 {prepare_parser_adoption_publication(&[source.clone(),target.clone()]).unwrap();}
        assert_eq!(source.mutation_sinks.borrow().len(),source_count);assert_eq!(target.mutation_sinks.borrow().len(),target_count);

        // Exercise real registry and observer admission across several owners,
        // including the hash table's small-capacity growth boundary. Assertions
        // concern unique observers, not any implementation-specific capacity.
        let mut owners=vec![source.clone(),target.clone()];let mut distinct=Vec::new();let mut registries=Vec::new();
        for _ in 0..6 {
            let document=eval(&mut engine,"new DOMParser().parseFromString('<main></main>','text/html')");
            let owner=engine.ctx().with_instance::<DomDocument,_>(&document,|document|document.realm.clone()).unwrap();
            let initial=CustomElementHub::new(owner.clone(),engine.ctx().deferred_microtasks());
            // Publish and retain the actual registry wrapper. Its canonical
            // native owner initializes the weak hub identity discovered by
            // adoption_hubs; a bare private HubState is not a registry.
            let registry=initial.registry_value(engine.ctx());
            let hub=engine.ctx().with_instance::<DomCustomElementRegistry,_>(&registry,|registry|registry.hub.clone()).unwrap();
            hub.observe_mutations().unwrap();
            owners.push(owner);distinct.push(hub);registries.push(registry);
        }
        prepare_parser_adoption_publication(&owners).unwrap();
        let counts=owners.iter().map(|owner|owner.mutation_sinks.borrow().len()).collect::<Vec<_>>();
        prepare_parser_adoption_publication(&owners).unwrap();
        assert_eq!(registries.len(),6);
        for (owner,count) in owners.iter().zip(counts) {
            assert_eq!(owner.mutation_sinks.borrow().len(),count);
            for hub in &distinct {
                assert_eq!(hub.attached_realms.borrow().iter().filter_map(std::rc::Weak::upgrade).filter(|realm|Rc::ptr_eq(realm,owner)).count(),1);
            }
        }
    }

    #[test]
    fn disabled_shadow_adoption_preserves_upgraded_definition_and_primary_registry() {
        let mut engine = Engine::new();
        let source = super::super::install(engine.ctx(), "<main></main>", 128).unwrap();
        eval(&mut engine, r#"
            class Blocked extends HTMLElement {static get disabledFeatures(){return ['shadow']}}
            customElements.define('x-policy',Blocked);
            var moving=document.createElement('x-policy');
            var parsed=new DOMParser().parseFromString('<main></main>','text/html');
            parsed.adoptNode(moving);
        "#);
        let hub = hub_from_ctx(engine.ctx()).unwrap();
        let parsed_value = eval(&mut engine, "parsed");
        let target = engine.ctx().with_instance::<DomDocument, _>(&parsed_value, |doc| doc.realm.clone()).unwrap();
        let target_hub = CustomElementHub::new(target.clone(), engine.ctx().deferred_microtasks());
        let mut permitted = hub.state.borrow().definitions.get("x-policy").unwrap().clone();
        permitted.disabled_shadow = false;
        target_hub.state.borrow_mut().definitions.insert(permitted.name.clone(), permitted);
        target_hub.observe_mutations().unwrap();
        // Reattaching the adopted source must not replace the target's registry.
        hub.observe_mutations_for(&target).unwrap();
        let moving_value = eval(&mut engine, "moving");
        let moving = engine.ctx().with_instance::<DomNode, _>(&moving_value, |node| node.id).unwrap();
        assert_eq!(target.session.borrow_mut().document_mut().attach_shadow(moving, lumen_html::ShadowMode::Open), Err(Error::WrongKind));
        let plain = target.session.borrow_mut().document_mut().create(NodeKind::Element {
            namespace: Namespace::Html, name: "x-policy".into(), attributes: vec![],
        }).unwrap();
        assert!(target.session.borrow_mut().document_mut().attach_shadow(plain, lumen_html::ShadowMode::Open).is_ok());
        assert!(source.session.borrow().document().kind(moving).is_err());
    }

    #[test]
    fn registry_upgrades_existing_elements_and_delivers_lifecycle_reactions() {
        let mut engine = Engine::new();
        let _realm =
            super::super::install(engine.ctx(), "<x-card value='before'></x-card>", 64).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
            var old = document.querySelector('x-card'), reactions = [];
            var defined = customElements.whenDefined('x-card'), definedValue = null;
            defined.then(value => definedValue = value);
            class XCard extends HTMLElement {
              static get observedAttributes() { return ['value']; }
              constructor() { super(); this.constructed = this.localName === 'x-card'; this.setAttribute('created', 'yes'); this.addEventListener('ping', () => reactions.push('listener')); }
              connectedCallback() { reactions.push('connected'); }
              attributeChangedCallback(name, before, after) { reactions.push(name + ':' + before + '>' + after); }
            }
            customElements.define('x-card', XCard);
            old instanceof XCard && old.constructed && old.getAttribute('created') === 'yes' &&
              customElements.get('x-card') === XCard &&
              customElements.getName(XCard) === 'x-card'
        "#
            ),
            Value::Bool(true)
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(&mut engine, "definedValue === XCard"),
            Value::Bool(true)
        ));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "old.dispatchEvent(new Event('ping')); reactions.join(',') === 'value:null>before,connected,listener'"
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(
                &mut engine,
                "old.setAttribute('value','after'); reactions.length === 4"
            ),
            Value::Bool(true)
        ));
        deliver_reactions(engine.ctx()).unwrap();
        let actual = eval(&mut engine, "reactions.join(',')");
        let Value::Str(actual) = actual else {
            panic!("custom-element reaction list was not a string");
        };
        assert_eq!(
            actual.as_str(),
            "value:null>before,connected,listener,value:before>after",
            "custom-element reaction order"
        );
    }

    #[test]
    fn registry_accepts_reflect_constructors_and_checks_called_interface_before_prototype() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "", 64).unwrap();
        assert!(matches!(eval(&mut engine, r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            function ReflectElement() { return Reflect.construct(HTMLElement, [], new.target); }
            customElements.define('x-reflect-element', ReflectElement);
            const reflected = new ReflectElement();
            if (!(reflected instanceof ReflectElement) ||
                Element.prototype.getAttribute.call(reflected, 'missing') !== null)
                throw new Error('plain Reflect constructor lost native Element backing');
            function ReflectParagraph() { return Reflect.construct(HTMLParagraphElement, [], new.target); }
            const paragraphProxy = new Proxy(ReflectParagraph, {});
            customElements.define('x-reflect-paragraph', paragraphProxy, {extends:'p'});
            const paragraph = new paragraphProxy();
            if (Object.getOwnPropertyDescriptor(Element.prototype, 'localName').get.call(paragraph) !== 'p')
                throw new Error('proxied customized Reflect constructor lost its tag');
            let armed = false, gets = 0;
            const wrong = new Proxy(function(){}, {get(target, key, receiver) {
                if (armed && key === 'prototype') { gets++; throw new Error('premature prototype'); }
                return Reflect.get(target, key, receiver);
            }});
            customElements.define('x-wrong-reflect', wrong, {extends:'p'});
            armed = true;
            let rejected = false;
            try { Reflect.construct(HTMLElement, [], wrong); } catch(error) { rejected = error instanceof TypeError; }
            check(rejected, 'wrong HTML interface must throw TypeError');
            check(gets === 0, 'wrong HTML interface must reject before new.target.prototype lookup: ' + gets);
            const registryGetter = Object.getOwnPropertyDescriptor(Element.prototype, 'customElementRegistry').get;
            // A valid plain function need not inherit from HTMLElement. Invoke
            // the actual native getter rather than ordinary prototype lookup.
            check(registryGetter.call(reflected) === customElements, 'Reflect constructor must retain native owning registry');
            return true;
        })()"#), Value::Bool(true)));
    }

    #[test]
    fn upgrade_failures_report_once_and_construction_stack_is_per_definition() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body><x-defined-failure></x-defined-failure></body>", 128).unwrap();
        assert!(matches!(eval(engine, r#"
            const reports = [];
            window.addEventListener('error', event => { reports.push(event.error); event.preventDefault(); });
            let attempts = 0;
            class DefinedFailure extends HTMLElement { constructor() { super(); attempts++; throw 'definition failure'; } }
            customElements.define('x-defined-failure', DefinedFailure);
            const failed = document.querySelector('x-defined-failure');
            if (reports.length !== 1 || reports[0] !== 'definition failure' || attempts !== 1)
                throw new Error('define must report and retain a failed upgrade');
            failed.remove(); document.body.appendChild(failed);
            if (attempts !== 1) throw new Error('failed element upgraded again');
            class Inner extends HTMLElement { constructor() { super(); this.inner = true; } }
            customElements.define('x-stack-inner', Inner);
            class Reentrant extends HTMLElement {
                constructor(skip) { super(); this.other = new Inner(); if (!skip) new Reentrant(true); }
            }
            customElements.define('x-stack-reentrant', Reentrant);
            const original = new Reentrant(true);
            const clone = original.cloneNode();
            if (reports.length !== 2 || !(reports[1] instanceof TypeError))
                throw new Error('already-constructed stack entry was reused');
            if (clone === original || original.other.customElementRegistry !== customElements)
                throw new Error('nested different definition lost identity');
            clone.remove(); document.body.appendChild(clone);
            reports.length === 2 && attempts === 1
        "#), Value::Bool(true)));
    }

    #[test]
    fn cloned_and_imported_elements_keep_owning_registry_identity_across_realms_and_gc() {
        let mut engine = Engine::new();
        let _realm = crate::install(engine.ctx(), "", 128).unwrap();
        let original = eval(&mut engine, r#"
            class ParentElement extends HTMLElement {}
            customElements.define('x-registry-owner', ParentElement);
            var registryOwner = document.createElement('x-registry-owner');
            registryOwner
        "#);
        let foreign_handle = engine.ctx().create_host_realm();
        let (_foreign_realm, imported, registry, constructor) = match engine.ctx().with_host_realm(&foreign_handle, |ctx| {
            let realm = crate::install(ctx, "", 128).unwrap();
            let global = ctx.global_object();
            ctx.member_set(&global, "parentElement", original).ok().expect("install parent element");
            let imported = ctx.eval_in_realm(&global, r#"
                class ForeignElement extends HTMLElement {}
                globalThis.ForeignElement = ForeignElement;
                customElements.define('x-registry-owner', ForeignElement);
                var foreignImported = document.importNode(parentElement);
                foreignImported
            "#).ok().expect("import into foreign registry");
            let constructor = ctx.member_get(&global, "ForeignElement").ok().expect("foreign element constructor");
            let registry = ctx.member_get(&global, "customElements").ok().expect("foreign registry");
            (realm, imported, registry, constructor)
        }) { Ok(values) => values, Err(_) => panic!("enter foreign registry realm") };
        let global = engine.ctx().global_object();
        for (name, value) in [("foreignImported", imported), ("foreignRegistry", registry), ("ForeignElement", constructor)] {
            engine.ctx().member_set(&global, name, value).ok().expect("install registry identity fixture");
        }
        assert!(matches!(eval(&mut engine, r#"
            if (foreignImported.customElementRegistry !== foreignRegistry || !(foreignImported instanceof ForeignElement))
                throw new Error('import must use the destination registry');
            if (registryOwner.cloneNode().customElementRegistry !== customElements)
                throw new Error('clone must retain its registry');
            var savedRegistry = customElements;
            customElements = null;
            registryOwner.customElementRegistry === savedRegistry && foreignImported.customElementRegistry !== savedRegistry
        "#), Value::Bool(true)));
        engine.collect_garbage();
        assert!(matches!(eval(&mut engine, "registryOwner.customElementRegistry === savedRegistry && foreignImported.customElementRegistry === foreignRegistry"), Value::Bool(true)));
    }

    #[test]
    fn registry_definition_transaction_preserves_getter_order_reentrancy_and_iterable_errors() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body></body>", 128).unwrap();
        assert!(matches!(eval(engine, r#"
            const log = [];
            function Protocol(){ return Reflect.construct(HTMLElement, [], new.target); }
            Protocol.prototype = new Proxy(Object.create(HTMLElement.prototype), {get(target,key,receiver){
                log.push(key); return Reflect.get(target,key,receiver);
            }});
            Protocol.prototype.attributeChangedCallback = function(){};
            Protocol.observedAttributes = new Set(['value']);
            Protocol.disabledFeatures = [];
            Protocol.formAssociated = true;
            const proxy = new Proxy(Protocol, {get(target,key,receiver){log.push(key);return Reflect.get(target,key,receiver)}});
            const oldIsArray = Array.isArray;
            Array.isArray = function(){throw new Error('author Array.isArray must not run')};
            customElements.define('x-protocol', proxy);
            Array.isArray = oldIsArray;
            if (log.join(',') !== 'prototype,connectedCallback,disconnectedCallback,connectedMoveCallback,adoptedCallback,attributeChangedCallback,observedAttributes,disabledFeatures,formAssociated,formAssociatedCallback,formResetCallback,formDisabledCallback,formStateRestoreCallback')
                throw new Error('definition getter order: '+log.join(','));
            let duplicateGets = 0;
            const duplicate = new Proxy(function(){}, {get(target,key,receiver){duplicateGets++;return Reflect.get(target,key,receiver)}});
            let duplicateError;
            try { customElements.define('x-protocol', duplicate); } catch(error) { duplicateError=error; }
            if (!(duplicateError instanceof DOMException) || duplicateError.name !== 'NotSupportedError' || duplicateGets !== 0)
                throw new Error('duplicate validation must precede prototype');
            let innerGets = 0;
            const inner = new Proxy(function(){}, {get(target,key,receiver){innerGets++;return Reflect.get(target,key,receiver)}});
            const outer = new Proxy(function(){}, {get(target,key,receiver){
                if (key === 'prototype') customElements.define('x-inner-definition',inner);
                return Reflect.get(target,key,receiver);
            }});
            let nestedError;
            try { customElements.define('x-outer-definition',outer); } catch(error) {nestedError=error;}
            if (!(nestedError instanceof DOMException) || nestedError.name !== 'NotSupportedError' || innerGets !== 0 || customElements.get('x-outer-definition') !== undefined)
                throw new Error('reentrant definition must stay atomic');
            const marker = new Error('conversion marker');
            let closes = 0;
            class IteratorFailure extends HTMLElement {
                static get observedAttributes(){ return { [Symbol.iterator](){ return {
                    next(){return {value:{toString(){throw marker}},done:false}},
                    return(){closes++;return {done:true}}
                }}}; }
                attributeChangedCallback(){}
            }
            let exactError;
            try { customElements.define('x-iterator-failure',IteratorFailure); } catch(error) {exactError=error;}
            if (exactError !== marker || closes !== 1 || customElements.get('x-iterator-failure') !== undefined)
                throw new Error('iterator failure must close and preserve original exception');
            class ValidAfterFailure extends HTMLElement {get formAssociatedCallback(){throw new Error('inactive form callback')}}
            customElements.define('x-iterator-failure',ValidAfterFailure);
            customElements.define('x-null-definition-options',class extends HTMLElement {},null);
            customElements.get('x-iterator-failure') === ValidAfterFailure && customElements.get('x-null-definition-options') !== undefined
        "#), Value::Bool(true)));
    }

    #[test]
    fn registry_when_defined_shares_pending_promise_and_rejects_invalid_names_asynchronously() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "", 64).unwrap();
        assert!(matches!(eval(engine, r#"
            var fulfilled = false, rejected = false, definedConstructor;
            const first = customElements.whenDefined('x-promise-definition');
            if (first !== customElements.whenDefined('x-promise-definition')) throw new Error('pending promise identity');
            first.then(value => {fulfilled=value===definedConstructor});
            customElements.whenDefined(null).catch(error => {rejected=error instanceof DOMException && error.name==='SyntaxError'});
            class PromiseElement extends HTMLElement {}
            definedConstructor = PromiseElement;
            customElements.define('x-promise-definition', PromiseElement);
            if (first === customElements.whenDefined('x-promise-definition')) throw new Error('resolved calls must create a new promise');
            Object.defineProperty(PromiseElement, "then", {get() {
                customElements.define("x-resolve-reentry", class extends HTMLElement {});
                return undefined;
            }});
            customElements.whenDefined("x-promise-definition");
            if (customElements.get("x-resolve-reentry") === undefined) throw new Error("resolution getter must reenter safely");
            !fulfilled && !rejected
        "#), Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval(engine, "fulfilled && rejected"), Value::Bool(true)));
        assert!(hub_from_ctx(engine.ctx()).unwrap().state.borrow().waiters.is_empty());
    }

    #[test]
    fn registry_reentrant_definition_skips_running_upgrade_and_uses_shadow_including_order() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body></body>", 512).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            const reports = [];
            window.addEventListener('error', event => {reports.push(event.error); event.preventDefault();});
            const outer = document.createElement('x-running-upgrade');
            const inner = document.createElement('x-defined-during-upgrade');
            document.body.append(outer, inner);
            let outerRuns = 0;
            class Inner extends HTMLElement {}
            class Outer extends HTMLElement {
                constructor() {
                    ++outerRuns;
                    customElements.define('x-defined-during-upgrade', Inner);
                    check(!(outer instanceof Outer), 'outer must wait for its super call');
                    check(inner instanceof Inner, 'nested definition upgrades its candidate');
                    super();
                }
            }
            customElements.define('x-running-upgrade', Outer);
            check(outerRuns === 1 && outer instanceof Outer && reports.length === 0, 'running upgrade must not recursively restart');
            const make = id => {const node = document.createElement('x-shadow-order'); node.id=id; return node;};
            const container = document.createElement('div'), host = document.createElement('div');
            const before = make('before'), light = make('light'), shadowChild = make('shadow'), nestedHost = make('nested-host'),
                nestedChild = make('nested-child'), after = make('after'), detached = make('detached');
            const shadow = host.attachShadow({mode:'closed'}), nested = nestedHost.attachShadow({mode:'closed'});
            host.append(light); shadow.append(shadowChild, nestedHost); nested.append(nestedChild);
            container.append(before, host, after); document.body.append(container);
            const calls = [];
            class Ordered extends HTMLElement {constructor() {super(); calls.push(this);}}
            customElements.define('x-shadow-order', Ordered);
            const expected = [before, shadowChild, nestedHost, nestedChild, light, after];
            check(calls.length === expected.length && calls.every((node, i) => node === expected[i]), 'closed nested shadow preorder identities');
            check(!(detached instanceof Ordered), 'definition does not upgrade disconnected candidates');
            customElements.upgrade(detached);
            check(detached instanceof Ordered && calls[calls.length-1] === detached, 'explicit detached upgrade');
            return true;
        })()"#), Value::Bool(true)));
    }

    #[test]
    fn move_before_preserves_native_state_and_queues_move_or_fallback_reactions_in_tree_order() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<body><div id='old'></div><div id='new'></div></body>", 512).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            const log = [];
            class Moved extends HTMLElement {
                connectedCallback() {log.push('connected:'+this.id);}
                disconnectedCallback() {log.push('disconnected:'+this.id);}
                connectedMoveCallback() {log.push('moved:'+this.id);}
            }
            class Legacy extends HTMLElement {
                connectedCallback() {log.push('connected:'+this.id);}
                disconnectedCallback() {log.push('disconnected:'+this.id);}
            }
            customElements.define('x-state-moved', Moved); customElements.define('x-state-legacy', Legacy);
            const old = document.getElementById('old'), destination = document.getElementById('new');
            const root = document.createElement('x-state-moved'); root.id='root';
            const shadow = root.attachShadow({mode:'closed'}), child = document.createElement('x-state-moved'); child.id='shadow';
            const legacy = document.createElement('x-state-legacy'); legacy.id='legacy';
            shadow.append(child, document.createElement("slot")); root.append(legacy);
            const input = document.createElement('input'); input.value='retained value'; root.append(input);
            const frame = document.createElement('iframe'); root.append(frame);
            old.append(root);
            globalThis.moveLog = log; globalThis.moveRoot = root; globalThis.moveInput = input;
            globalThis.moveOld = old; globalThis.moveDestination = destination; globalThis.moveFrame = frame;
            globalThis.moveWindow = frame.contentWindow; globalThis.moveDocument = frame.contentDocument;
            const marker = moveDocument.createElement('p'); marker.id='kept'; moveDocument.body.append(marker);
            input.focus();
            check(document.activeElement === input, 'initial native focus');
            const range = document.createRange(); range.selectNodeContents(legacy);
            const iterator = document.createNodeIterator(old, NodeFilter.SHOW_ELEMENT);
            iterator.nextNode(); iterator.nextNode(); iterator.nextNode();
            globalThis.moveRange = range; globalThis.moveIterator = iterator;
            return true;
        })()"#), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            moveLog.length=0;
            const observer = new MutationObserver(() => {});
            observer.observe(moveOld,{childList:true}); observer.observe(moveDestination,{childList:true});
            moveDestination.moveBefore(moveRoot, null);
            const records = observer.takeRecords(); observer.disconnect();
            if (records.length !== 2 || records[0].target !== moveOld || records[0].removedNodes[0] !== moveRoot ||
                records[1].target !== moveDestination || records[1].addedNodes[0] !== moveRoot) throw new Error('move observer records');
            if (document.activeElement !== moveInput || moveInput.value !== 'retained value' ||
                moveFrame.contentWindow !== moveWindow || moveFrame.contentDocument !== moveDocument ||
                moveDocument.getElementById('kept') === null) throw new Error('native focus/control/iframe state lost');
            if (moveRange.startContainer !== moveOld || moveRange.startOffset !== 0 ||
                moveIterator.nextNode() !== null) throw new Error('standard live Range/NodeIterator pre-remove updates');
            return true;
        })()"#), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, "moveLog.join(',') === 'moved:root,moved:shadow,disconnected:legacy,connected:legacy'"), Value::Bool(true)));
        assert!(realm.moving_node.get().is_none());
    }

    #[test]
    fn move_before_validates_typed_parent_interfaces_and_same_root_before_mutation() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "<body><div id='left'><p id='child'></p></div><div id='right'></div></body>", 256).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            const left = document.getElementById('left'), right = document.getElementById('right'), child = document.getElementById('child');
            check(!('moveBefore' in Node.prototype) && !('moveBefore' in document.createTextNode('x')) &&
                typeof DocumentFragment.prototype.moveBefore === 'function' && !("moveBefore" in Element.prototype[Symbol.unscopables]),
                'ParentNode interface placement');
            const failure = (action, name) => {let caught=false;try {action();} catch(error) {
                caught = name === 'TypeError' ? error instanceof TypeError : error instanceof DOMException && error.name === name;
            } check(caught, 'move exception '+name); check(child.parentNode === left, 'failure must be atomic');};
            failure(() => right.moveBefore(null, null), 'TypeError');
            failure(() => right.moveBefore(child), 'TypeError');
            failure(() => right.moveBefore(child, {}), 'TypeError');
            failure(() => right.moveBefore(child, left), 'NotFoundError');
            failure(() => child.moveBefore(left, null), 'HierarchyRequestError');
            failure(() => document.createElement('div').moveBefore(child, null), 'HierarchyRequestError');
            const foreign = document.implementation.createHTMLDocument('foreign');
            failure(() => foreign.body.moveBefore(child, null), 'HierarchyRequestError');
            failure(() => right.moveBefore(child, foreign.body), 'NotFoundError');
            failure(() => document.moveBefore(document.documentElement, null), 'HierarchyRequestError');
            right.moveBefore(child, undefined);
            check(child.parentNode === right, 'explicit undefined is a nullable reference');
            const detached = document.createDocumentFragment(), a = document.createElement('div'), b = document.createElement('div'),
                comment = document.createComment('retained');
            detached.append(a, b); a.append(comment); b.moveBefore(comment, null);
            check(comment.parentNode === b && !comment.isConnected && comment.data === 'retained', 'same-root disconnected CharacterData move');
            right.moveBefore(child, child);
            check(child.parentNode === right && child.ownerDocument === document, 'same-node reference');
            return true;
        })()"#), Value::Bool(true)));
        assert!(realm.moving_node.get().is_none());
    }

    #[test]
    fn registry_validates_names_constructors_and_observed_attributes() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "", 32).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
            class Valid extends HTMLElement {}
            var invalidName = false, invalidConstructor = false, invalidArrow = false, invalidObserved = false, duplicate = false;
            try { customElements.define('badname', Valid); } catch (error) { invalidName = error.name === 'SyntaxError'; }
            try { customElements.define('x-not-element', null); } catch (error) { invalidConstructor = error.name === 'TypeError'; }
            try { customElements.define('x-not-constructor', () => {}); } catch (error) { invalidArrow = error.name === 'TypeError'; }
            class BadObserved extends HTMLElement { static get observedAttributes() { return 'value'; } attributeChangedCallback() {} }
            try { customElements.define('x-bad-observed', BadObserved); } catch (error) { invalidObserved = error.name === 'TypeError'; }
            customElements.define('x-valid', Valid);
            try { customElements.define('x-valid', class extends HTMLElement {}); } catch (error) { duplicate = error.name === 'NotSupportedError'; }
            invalidName && invalidConstructor && invalidArrow && invalidObserved && duplicate && customElements.get('x-valid') === Valid
        "#
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn specification_upgrade_snapshots_precede_constructor_mutations_and_failures_clear_reactions() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body><x-snapshot value='before'></x-snapshot><x-disabled-shadow></x-disabled-shadow><x-broken-snapshot value='initial'></x-broken-snapshot></body>", 256).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            globalThis.upgradeLog = []; globalThis.upgradeReports = [];
            window.addEventListener('error', event => {upgradeReports.push(event.error); event.preventDefault();});
            const original = document.querySelector('x-snapshot');
            class Snapshot extends HTMLElement {
                static observedAttributes = ['value'];
                constructor() {super(); this.setAttribute('value', 'after');}
                connectedCallback() {upgradeLog.push('connected');}
                attributeChangedCallback(name, oldValue, newValue, namespace) {
                    upgradeLog.push(name + ':' + oldValue + '>' + newValue + ':' + namespace);
                }
            }
            customElements.define('x-snapshot', Snapshot);
            check(original instanceof Snapshot && original.getAttribute('value') === 'after', 'upgrade retained original identity');
            const blocked = document.querySelector('x-disabled-shadow'); blocked.attachShadow({mode:'closed'});
            globalThis.blockedConstructors = 0;
            class DisabledShadow extends HTMLElement {
                static disabledFeatures = ['shadow'];
                constructor() {super(); blockedConstructors++;}
            }
            customElements.define('x-disabled-shadow', DisabledShadow);
            check(blockedConstructors === 0 && upgradeReports.length === 1 &&
                upgradeReports[0] instanceof DOMException && upgradeReports[0].name === 'NotSupportedError', 'existing shadow root rejects upgrade before constructor');
            class BrokenSnapshot extends HTMLElement {
                static observedAttributes = ['value'];
                constructor() {super(); this.valueMarker = 'mutated'; throw 'failed-snapshot';}
                connectedCallback() {upgradeLog.push('failed-connected');}
                attributeChangedCallback() {upgradeLog.push('failed-attribute');}
            }
            customElements.define('x-broken-snapshot', BrokenSnapshot);
            customElements.upgrade(blocked);
            check(upgradeReports.length === 2 && upgradeReports[1] === 'failed-snapshot' && blockedConstructors === 0, 'failed upgrade is reported once');
            return true;
        })()"#), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, "if (upgradeLog.join(',') !== 'value:null>before:null,connected,value:before>after:null') throw new Error('initial snapshots and constructor mutations: ' + upgradeLog); true"), Value::Bool(true)));
    }

    #[test]
    fn specification_fresh_autonomous_construction_uses_returned_identity_without_upgrade_stack() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body></body>", 256).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            let recurse = true, nested, outer;
            class BeforeSuper extends HTMLElement {
                constructor() {
                    if (recurse) { recurse = false; nested = new BeforeSuper(); }
                    super(); outer = this;
                }
            }
            customElements.define('x-before-super', BeforeSuper);
            const a = document.createElement('x-before-super');
            check(a === outer && a !== nested && nested.parentNode === null,
                'fresh construction has no provisional upgrade stack entry');
            let alternate = true, first, returned;
            class Alternate extends HTMLElement {
                constructor() {
                    super();
                    if (alternate) { alternate = false; first = this;
                        returned = new Alternate(); return returned; }
                }
            }
            customElements.define('x-alternate', Alternate);
            const b = document.createElement('x-alternate');
            check(b === returned && b !== first && first.parentNode === null,
                'actual returned autonomous instance is used');
            let invalid;
            window.addEventListener('error', event => event.preventDefault());
            class Invalid extends HTMLElement {
                constructor() { super(); invalid = this; this.setAttribute('unexpected', 'yes'); }
            }
            customElements.define('x-invalid-fresh', Invalid);
            const fallback = document.createElement('x-invalid-fresh');
            check(fallback !== invalid && fallback instanceof HTMLUnknownElement &&
                fallback.attributes.length === 0 && invalid.getAttribute('unexpected') === 'yes',
                'failure fallback preserves the author-held constructed object');
            let calls = 0;
            const scoped = new CustomElementRegistry();
            class NonHtml extends HTMLElement { constructor() {super(); calls++;} }
            scoped.define('x-non-html', NonHtml);
            const nonHtml = new Document().createElement('x-non-html', {customElementRegistry:scoped});
            check(calls === 0 && nonHtml.namespaceURI === null && nonHtml.customElementRegistry === scoped,
                'definition lookup excludes non-HTML namespace while preserving registry association');
            return true;
        })()"#), Value::Bool(true)));
    }

    #[test]
    fn specification_reaction_element_queues_preserve_reentrant_order_and_report_each_exception() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(), "", 256).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            globalThis.queueLog = []; globalThis.callbackReports = [];
            window.addEventListener('error', event => {callbackReports.push(event.error); event.preventDefault();});
            class QueuedElement extends HTMLElement {
                static observedAttributes = ['value'];
                attributeChangedCallback(name, before, value) {
                    queueLog.push(this.label + value);
                    if (this.label === 'B') queueA.setAttribute('value','3');
                    if (this.label === 'A' && value === '1') throw 'first-callback';
                    if (this.label === 'C') throw 'last-callback';
                }
            }
            customElements.define('x-element-queue', QueuedElement);
            globalThis.queueA = new QueuedElement(); queueA.label = 'A';
            globalThis.queueB = new QueuedElement(); const b = queueB; b.label = 'B';
            globalThis.queueC = new QueuedElement(); const c = queueC; c.label = 'C';
            return true;
        })()"#), Value::Bool(true)));
// Host edits bypass IDL operations and therefore use the real backup
// element queue; authored mutations below exercise nested IDL scopes.
let nodes: Vec<_> = ["queueA", "queueB", "queueA", "queueC"].into_iter().map(|name| {
    let value = eval(engine, name);
    engine.ctx().with_instance::<DomNode, _>(&value, |node| node.id).unwrap()
}).collect();
for (node, value) in nodes.into_iter().zip(["1", "1", "2", "1"]) {
    realm.session.borrow_mut().document_mut().set_attribute(node, "value", value).unwrap();
}
assert!(matches!(eval(engine, "queueLog.length === 0"), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, r#"
            if (queueLog.join(',') !== 'A1,A2,B1,A3,C1') throw new Error('per-element queue order: '+queueLog);
            if (callbackReports.join(',') !== 'first-callback,last-callback') throw new Error('callback reporting: '+callbackReports);
            true
        "#), Value::Bool(true)));
    }

    #[test]
    fn specification_definition_function_conversion_and_html_constructor_identity_are_exact() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "", 128).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            const check = (ok, message) => {if (!ok) throw new Error(message);};
            class NullCallback extends HTMLElement {}
            NullCallback.prototype.connectedCallback = null;
            let rejected = false; try {customElements.define('x-null-callback', NullCallback);} catch(error) {rejected = error instanceof TypeError;}
            check(rejected && customElements.get('x-null-callback') === undefined, 'null Function conversion must fail atomically');
            NullCallback.prototype.connectedCallback = undefined;
            customElements.define('x-null-callback', NullCallback);
            let formGets = 0;
            function Paragraph() {return Reflect.construct(HTMLParagraphElement, [], new.target);}
            Object.defineProperty(Paragraph,'formAssociated',{get() {formGets++; return false;}});
            customElements.define('x-paragraph-form-get', Paragraph, {extends:'p'});
            check(formGets === 1, 'formAssociated Get applies to customized built-ins');
            customElements.define('x-native-constructor', HTMLElement);
            rejected = false; try {new HTMLElement();} catch(error) {rejected = error instanceof TypeError;}
            check(rejected, 'active function equal to new.target must reject even if registered');
            function ProxyElement() {return Reflect.construct(HTMLElement, [], new.target);}
            const proxy = new Proxy(ProxyElement, {});
            customElements.define('x-proxy-constructor', proxy);
            const element = new proxy();
            check(element instanceof ProxyElement && Element.prototype.getAttribute.call(element,'missing') === null, 'distinct proxy new.target remains constructible');
            return true;
        })()"#), Value::Bool(true)));
    }

    #[test]
    fn specification_connection_reactions_follow_changed_shadow_subtrees_and_ordinary_reparenting() {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        let engine = runtime.engine();
        let _realm = crate::install(engine.ctx(), "<body><main id='left'></main><main id='right'></main></body>", 256).unwrap();
        assert!(matches!(eval(engine, r#"(() => {
            globalThis.connectionLog = [];
            class TreeElement extends HTMLElement {
                connectedCallback() {connectionLog.push(this.label + '+');}
                disconnectedCallback() {connectionLog.push(this.label + '-');}
            }
            customElements.define('x-reaction-tree', TreeElement);
            globalThis.treeHost = new TreeElement(); treeHost.label = 'H';
            const shadowChild = new TreeElement(); shadowChild.label = 'S';
            const nestedShadowChild = new TreeElement(); nestedShadowChild.label = 'T';
            const lightChild = new TreeElement(); lightChild.label = 'L';
            shadowChild.attachShadow({mode:'closed'}).append(nestedShadowChild);
            treeHost.attachShadow({mode:'closed'}).append(shadowChild);
            treeHost.append(lightChild);
            document.getElementById('left').append(treeHost);
            return true;
        })()"#), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, "if (connectionLog.join(',') !== 'H+,S+,T+,L+') throw new Error('shadow connection order: '+connectionLog); connectionLog.length=0; document.getElementById('right').append(treeHost); true"), Value::Bool(true)));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(eval(engine, "if (connectionLog.join(',') !== 'H-,H+,S-,S+,T-,T+,L-,L+') throw new Error('ordinary reparent per-element reactions: '+connectionLog); true"), Value::Bool(true)));
    }

    #[test]
    fn specification_reaction_queue_admission_is_bounded_and_quiet_storage_is_reclaimed() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "", 32).unwrap();
        let key = RealmNode::new(&realm, realm.session.borrow().document().root());
        let mut queue = ReactionQueue::default();
        for _ in 0..100_000 {queue.push_back(Reaction::Connected(key, Value::Undefined));}
        queue.push_back(Reaction::Connected(key, Value::Undefined));
        assert_eq!(queue.len(), 100_000);
        assert_eq!(queue.elements.len(), 100_000);
        assert_eq!(queue.queues.len(), 1);
        assert!(queue.allocation_failed);
        let mut delivered = 0;
        while queue.pop_front().is_some() {delivered += 1;}
        assert_eq!(delivered, 100_000);
        assert_eq!(queue.len(), 0);
        assert!(queue.current.is_none());
        assert!(queue.queues.capacity() <= 256);
        assert!(queue.elements.capacity() <= 256);
    }

    #[test]
    fn direct_and_nested_construction_keep_each_element_and_report_throws() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
            class InnerCard extends HTMLElement { constructor() { super(); this.kind = 'inner'; } }
            class OuterCard extends HTMLElement { constructor() { super(); this.inner = new InnerCard(); this.kind = 'outer'; } }
            class ThrowCard extends HTMLElement { constructor() { super(); throw new Error('expected'); } }
            customElements.define('x-inner-card', InnerCard);
            customElements.define('x-outer-card', OuterCard);
            customElements.define('x-throw-card', ThrowCard);
            var outer = new OuterCard(), caught = false;
            try { new ThrowCard(); } catch (error) { caught = error.message === 'expected'; }
            document.querySelector('main').appendChild(outer);
            outer instanceof OuterCard && outer.inner instanceof InnerCard && outer.kind === 'outer' &&
              outer.inner.kind === 'inner' && document.querySelector('x-outer-card') === outer && caught
        "#
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn customized_builtin_upgrade_preserves_tag_interface_and_is_attribute() {
        let mut engine = Engine::new();
        let _realm =
            super::super::install(engine.ctx(), "<input is='x-fancy-input' value='seed'>", 64)
                .unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
                var input = document.querySelector('input');
                class FancyInput extends HTMLInputElement {
                  constructor() { super(); this.createdByCustomCtor = this.localName === 'input'; }
                  connectedCallback() { this.setAttribute('connected-custom', 'yes'); }
                }
                customElements.define('x-fancy-input', FancyInput, {extends:'input'});
                input instanceof HTMLInputElement && input instanceof FancyInput &&
                  input.localName === 'input' && input.getAttribute('is') === 'x-fancy-input' &&
                  input.createdByCustomCtor && customElements.getName(FancyInput) === 'x-fancy-input'
            "#,
            ),
            Value::Bool(true)
        ));
        deliver_reactions(engine.ctx()).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "input.getAttribute('connected-custom') === 'yes'",
            ),
            Value::Bool(true)
        ));
        assert!(matches!(
            eval(
                &mut engine,
                r#"var fresh = document.createElement('input', {is:'x-fancy-input'}); fresh instanceof HTMLInputElement && fresh instanceof FancyInput && fresh.localName === 'input' && !fresh.hasAttribute('is') && fresh.outerHTML.startsWith('<input is="x-fancy-input"') && fresh.createdByCustomCtor"#,
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn customized_builtins_cover_each_exposed_form_interface() {
        let mut engine = Engine::new();
        let _realm = super::super::install(
            engine.ctx(),
            "<select is='x-fancy-select'></select><option is='x-fancy-option'></option><textarea is='x-fancy-textarea'></textarea>",
            64,
        )
        .unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
                class FancySelect extends HTMLSelectElement { constructor() { super(); this.ready = this.localName === 'select'; } }
                class FancyOption extends HTMLOptionElement { constructor() { super(); this.ready = this.localName === 'option'; } }
                class FancyTextArea extends HTMLTextAreaElement { constructor() { super(); this.ready = this.localName === 'textarea'; } }
                customElements.define('x-fancy-select', FancySelect, {extends:'select'});
                customElements.define('x-fancy-option', FancyOption, {extends:'option'});
                customElements.define('x-fancy-textarea', FancyTextArea, {extends:'textarea'});
                var select = document.querySelector('select'), option = document.querySelector('option'), textarea = document.querySelector('textarea');
                select instanceof HTMLSelectElement && select instanceof FancySelect && select.ready &&
                  option instanceof HTMLOptionElement && option instanceof FancyOption && option.ready &&
                  textarea instanceof HTMLTextAreaElement && textarea instanceof FancyTextArea && textarea.ready
            "#,
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn document_import_node_copies_between_realms_without_mutating_source() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
                var sourceDocument = new DOMParser().parseFromString('<root xmlns:x="urn:example" x:key="value"><child>text</child></root>', 'application/xml');
                var source = sourceDocument.documentElement;
                var imported = document.importNode(source, true);
                var shallow = document.importNode(source, false);
                var rejectedDocument = false;
                try { document.importNode(sourceDocument, true); } catch (error) { rejectedDocument = error.name === 'NotSupportedError'; }
                imported !== source && imported.ownerDocument === document && source.ownerDocument === sourceDocument &&
                  imported.getAttributeNS('urn:example', 'key') === 'value' && imported.firstChild.textContent === 'text' &&
                  shallow.ownerDocument === document && shallow.firstChild === null && source.firstChild.textContent === 'text' &&
                  rejectedDocument
            "#,
            ),
            Value::Bool(true)
        ));
    }
}
