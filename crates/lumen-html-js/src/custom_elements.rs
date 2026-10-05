//! Custom element definitions, upgrades, and lifecycle reaction collection.
//!
//! The JS adapter owns constructor values and callbacks; node identity and tree mutations remain
//! in `lumen-html`. The hub is installed once per interpreter and attached to the realm's
//! mutation fanout, so custom-element reactions do not replace mutation/observer consumers.
use super::*;
use lumen::embed::{Deferred, JsFunction, JsHost};
use lumen_bind::{CtorRet, Host, IntoError};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone)]
struct Definition {
    name: String,
    extends: Option<String>,
    constructor: Value,
    observed_attributes: HashSet<String>,
    callbacks: HashMap<&'static str, Value>,
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

#[derive(Clone)]
enum Reaction {
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
}

#[derive(Default)]
struct HubState {
    definitions: HashMap<String, Definition>,
    waiters: HashMap<String, Vec<Deferred>>,
    pending_upgrade: Vec<PendingUpgrade>,
    upgraded: HashMap<RealmNode, String>,
    failed: HashSet<RealmNode>,
    connected: HashMap<RealmNode, bool>,
    reactions: VecDeque<Reaction>,
}

struct PendingUpgrade {
    realm: Rc<DomRealm>,
    node: NodeId,
    consumed_by_super: bool,
}

#[derive(Clone)]
pub(crate) struct CustomElementHub {
    realm: Rc<DomRealm>,
    // Hubs are cloned into the registry and reaction-delivery native instances. Keep realm
    // registration shared so a realm attached while adopting nodes is visible to the delivery
    // clone that later resolves callback receivers.
    attached_realms: Rc<RefCell<Vec<std::rc::Weak<DomRealm>>>>,
    state: Rc<RefCell<HubState>>,
    jobs: Rc<RefCell<Vec<Value>>>,
    delivery: Rc<RefCell<Option<Value>>>,
    scheduled: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct HubSlot(Rc<RefCell<Option<CustomElementHub>>>);

#[lumen_bind::class(name = "CustomElementRegistry", hint(js(webidl)))]
pub(crate) struct DomCustomElementRegistry {
    hub: CustomElementHub,
}

#[lumen_bind::class(name = "CustomElementReactionDelivery")]
struct ReactionDelivery {
    hub: CustomElementHub,
}

#[lumen_bind::methods]
impl DomCustomElementRegistry {
    fn define(
        &self,
        ctx: &mut Ctx,
        name: &str,
        constructor: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        self.hub.define(ctx, name, constructor, options)
    }

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

    fn get_name(&self, ctx: &mut Ctx, constructor: Value) -> OpResult<Value> {
        let found = self
            .hub
            .state
            .borrow()
            .definitions
            .values()
            .find(|definition| ctx.values_strict_equal(&definition.constructor, &constructor))
            .map(|definition| Value::from_string(definition.name.clone()))
            .unwrap_or(Value::Undefined);
        Ok(found)
    }

    fn when_defined(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        if !valid_name(name) {
            return Err(OpError::new("SyntaxError", "invalid custom element name"));
        }
        if let Some(definition) = self.hub.state.borrow().definitions.get(name) {
            let deferred = Deferred::new(ctx);
            let promise = deferred.promise();
            deferred.resolve(ctx, definition.constructor.clone());
            return Ok(promise);
        }
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        self.hub
            .state
            .borrow_mut()
            .waiters
            .entry(name.into())
            .or_default()
            .push(deferred);
        Ok(promise)
    }

    fn upgrade(&self, ctx: &mut Ctx, root: &DomNode) -> OpResult<()> {
        if !self.hub.owns_realm(&root.realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "root belongs to another document",
            ));
        }
        self.hub.upgrade_subtree(ctx, &root.realm, root.id)
    }
}

#[lumen_bind::methods]
impl ReactionDelivery {
    fn flush(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.hub.flush(ctx)
    }
}

impl CustomElementHub {
    pub(crate) fn new(realm: Rc<DomRealm>, jobs: Rc<RefCell<Vec<Value>>>) -> Self {
        Self {
            // `observe_mutations_for` both records the realm and installs its
            // sink. Start empty so installation wires the initial realm too.
            attached_realms: Rc::new(RefCell::new(Vec::new())),
            realm,
            state: Rc::new(RefCell::new(HubState::default())),
            jobs,
            delivery: Rc::new(RefCell::new(None)),
            scheduled: Rc::new(Cell::new(false)),
        }
    }

    pub(crate) fn registry_value(&self, ctx: &mut Ctx) -> Value {
        ctx.new_instance(DomCustomElementRegistry { hub: self.clone() })
    }

    /// Add the custom-element observer to the realm's existing mutation fanout.
    pub(crate) fn observe_mutations(&self) {
        self.observe_mutations_for(&self.realm.clone());
    }

    fn observe_mutations_for(&self, attached: &Rc<DomRealm>) {
        {
            let mut realms = self.attached_realms.borrow_mut();
            realms.retain(|realm| realm.strong_count() != 0);
            if realms
                .iter()
                .filter_map(std::rc::Weak::upgrade)
                .any(|realm| Rc::ptr_eq(&realm, attached))
            {
                return;
            }
            realms.push(Rc::downgrade(attached));
        }
        let weak = Rc::downgrade(&self.state);
        let realm = Rc::downgrade(attached);
        let jobs = self.jobs.clone();
        let delivery = self.delivery.clone();
        let scheduled = self.scheduled.clone();
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
                if let Some(definition_name) = state.upgraded.get(&key).cloned() {
                    if let Some(definition) = state.definitions.get(&definition_name) {
                        if definition
                            .observed_attributes
                            .iter()
                            .any(|attribute| attribute == name)
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
            // Connection transitions are computed against the post-mutation tree, while the
            // prior state is retained per upgraded node. This handles reparenting without
            // emitting a spurious disconnect/connect pair for a node that remains connected.
            let nodes: Vec<RealmNode> = state
                .upgraded
                .keys()
                .copied()
                .filter(|node| node.realm == Rc::as_ptr(&attached) as usize)
                .collect();
            for node in nodes {
                let now_connected = tree_connected(document, node.node);
                let was_connected = state.connected.get(&node).copied().unwrap_or(false);
                if now_connected != was_connected {
                    state.connected.insert(node, now_connected);
                    let callback = state
                        .upgraded
                        .get(&node)
                        .and_then(|name| state.definitions.get(name))
                        .and_then(|definition| {
                            definition.callbacks.get(if now_connected {
                                "connectedCallback"
                            } else {
                                "disconnectedCallback"
                            })
                        })
                        .cloned()
                        .unwrap_or(Value::Undefined);
                    if now_connected {
                        state
                            .reactions
                            .push_back(Reaction::Connected(node, callback));
                    } else {
                        state
                            .reactions
                            .push_back(Reaction::Disconnected(node, callback));
                    }
                }
            }
            let queued = state.reactions.len() != before;
            drop(state);
            if queued && !scheduled.replace(true) {
                if let Some(callback) = delivery.borrow().as_ref() {
                    jobs.borrow_mut().push(callback.clone());
                }
            }
        }));
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
        if !valid_name(name) {
            return Err(OpError::new("SyntaxError", "invalid custom element name"));
        }
        if !constructor.is_callable() {
            return Err(OpError::new(
                "TypeError",
                "custom element constructor must be a constructor",
            ));
        }
        let extends = if let Some(options) = options {
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
            if custom_name_tag(tag).is_none() {
                return Err(OpError::new(
                    "NotSupportedError",
                    "customized built-in base interface is not implemented",
                ));
            }
        }
        let prototype = ctx
            .member_get(&constructor, "prototype")
            .map_err(OpError::thrown)?;
        if !matches!(prototype, Value::Obj(_)) {
            return Err(OpError::new(
                "TypeError",
                "custom element prototype must be an object",
            ));
        }
        let global = ctx.global_this();
        let interface = extends
            .as_deref()
            .and_then(custom_name_tag)
            .unwrap_or("HTMLElement");
        let html_constructor = ctx
            .member_get(&global, interface)
            .map_err(OpError::thrown)?;
        let html_prototype = ctx
            .member_get(&html_constructor, "prototype")
            .map_err(OpError::thrown)?;
        let mut ancestor = prototype.clone();
        let mut extends_html_element = false;
        for _ in 0..256 {
            if ctx.values_strict_equal(&ancestor, &html_prototype) {
                extends_html_element = true;
                break;
            }
            ancestor = ctx.prototype_of(&ancestor);
            if matches!(ancestor, Value::Null | Value::Undefined) {
                break;
            }
        }
        if !extends_html_element {
            return Err(OpError::new(
                "TypeError",
                "custom element constructor does not extend its required HTML interface",
            ));
        }
        let mut callbacks = HashMap::new();
        for name in [
            "connectedCallback",
            "disconnectedCallback",
            "adoptedCallback",
            "attributeChangedCallback",
            "formAssociatedCallback",
            "formDisabledCallback",
            "formResetCallback",
            "formStateRestoreCallback",
        ] {
            let callback = ctx.member_get(&prototype, name).map_err(OpError::thrown)?;
            if !matches!(callback, Value::Undefined | Value::Null) && !callback.is_callable() {
                return Err(OpError::new(
                    "TypeError",
                    format!("{name} must be callable"),
                ));
            }
            if callback.is_callable() {
                callbacks.insert(name, callback);
            }
        }
        let observed_attributes = if callbacks.contains_key("attributeChangedCallback") {
            match ctx.member_get(&constructor, "observedAttributes") {
                Ok(Value::Undefined) => HashSet::new(),
                Ok(value) => {
                    let global = ctx.global_this();
                    let array_ctor = ctx.member_get(&global, "Array").map_err(OpError::thrown)?;
                    let is_array = ctx
                        .member_get(&array_ctor, "isArray")
                        .map_err(OpError::thrown)?;
                    let is_array = ctx
                        .invoke(is_array, array_ctor, &[value.clone()])
                        .map_err(OpError::thrown)?;
                    if !matches!(is_array, Value::Bool(true)) {
                        return Err(OpError::new(
                            "TypeError",
                            "observedAttributes must be an array",
                        ));
                    }
                    let length = ctx.member_get(&value, "length").map_err(OpError::thrown)?;
                    let length = ctx.coerce_number(&length).map_err(OpError::thrown)?;
                    if !length.is_finite()
                        || length < 0.0
                        || length > 65_536.0
                        || length.fract() != 0.0
                    {
                        return Err(OpError::new(
                            "TypeError",
                            "observedAttributes must be an array-like value",
                        ));
                    }
                    let mut attributes = HashSet::new();
                    for index in 0..(length as usize) {
                        let item = ctx
                            .member_get(&value, &index.to_string())
                            .map_err(OpError::thrown)?;
                        attributes.insert(
                            ctx.coerce_string(&item)
                                .map_err(OpError::thrown)?
                                .to_string(),
                        );
                    }
                    attributes
                }
                Err(error) => return Err(OpError::thrown(error)),
            }
        } else {
            HashSet::new()
        };
        let definition = Definition {
            name: name.into(),
            extends,
            constructor: constructor.clone(),
            observed_attributes,
            callbacks,
        };
        let waiters = {
            let mut state = self.state.borrow_mut();
            if state.definitions.contains_key(name)
                || state
                    .definitions
                    .values()
                    .any(|item| ctx.values_strict_equal(&item.constructor, &constructor))
            {
                return Err(OpError::new(
                    "NotSupportedError",
                    "custom element name or constructor is already registered",
                ));
            }
            state.definitions.insert(name.into(), definition);
            state.waiters.remove(name).unwrap_or_default()
        };
        for waiter in waiters {
            waiter.resolve(ctx, constructor.clone());
        }
        let realm = self.realm.clone();
        let root = realm.session.borrow().document().root();
        self.upgrade_subtree(ctx, &realm, root)?;
        Ok(())
    }

    fn upgrade_subtree(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId) -> OpResult<()> {
        if !self.owns_realm(realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "root is not owned by this custom element registry",
            ));
        }
        let candidates = {
            let session = realm.session.borrow();
            let document = session.document();
            let mut ids = Vec::new();
            let mut stack = vec![root];
            while let Some(id) = stack.pop() {
                ids.push(id);
                let mut descendants = children(document, id).map_err(dom_error)?;
                if let Some(shadow) = document.shadow_root(id).map_err(dom_error)? {
                    descendants.extend(children(document, shadow).map_err(dom_error)?);
                }
                if let Some(content) = document.template_content(id).map_err(dom_error)? {
                    descendants.extend(children(document, content).map_err(dom_error)?);
                }
                stack.extend(descendants.into_iter().rev());
            }
            ids.into_iter()
                .filter_map(|id| match document.kind(id).ok()? {
                    NodeKind::Element {
                        namespace: Namespace::Html,
                        name,
                        attributes,
                    } => Some((
                        id,
                        name.to_string(),
                        attributes
                            .iter()
                            .find(|(attribute, _)| attribute == "is")
                            .map(|(_, value)| value.clone()),
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        for (id, local_name, is_value) in candidates {
            let key = RealmNode::new(&realm, id);
            let constructor = {
                let state = self.state.borrow();
                if state.upgraded.contains_key(&key) || state.failed.contains(&key) {
                    continue;
                }
                state
                    .definitions
                    .values()
                    .find(|definition| match definition.extends.as_deref() {
                        Some(base_tag) => {
                            base_tag == local_name.as_str()
                                && is_value.as_deref() == Some(definition.name.as_str())
                        }
                        None => definition.name.as_str() == local_name.as_str(),
                    })
                    .map(|definition| (definition.name.clone(), definition.constructor.clone()))
            };
            if let Some((custom_name, constructor)) = constructor {
                let expected = realm.wrap(ctx, id);
                self.state
                    .borrow_mut()
                    .pending_upgrade
                    .push(PendingUpgrade {
                        realm: realm.clone(),
                        node: id,
                        consumed_by_super: false,
                    });
                let result = ctx.construct_value(constructor, &[]);
                let consumed = self
                    .state
                    .borrow_mut()
                    .pending_upgrade
                    .pop()
                    .map(|pending| pending.consumed_by_super)
                    .unwrap_or(false);
                let constructed = match result {
                    Ok(value) => value,
                    Err(error) => {
                        let mut state = self.state.borrow_mut();
                        state.upgraded.remove(&key);
                        state.connected.remove(&key);
                        state.reactions.retain(|reaction| match reaction {
                            Reaction::Connected(node, _)
                            | Reaction::Disconnected(node, _)
                            | Reaction::Attribute(node, ..)
                            | Reaction::Adopted(node, ..) => *node != key,
                        });
                        state.failed.insert(key);
                        return Err(OpError::thrown(error));
                    }
                };
                if !ctx.values_strict_equal(&constructed, &expected) {
                    let mut state = self.state.borrow_mut();
                    state.upgraded.remove(&key);
                    state.connected.remove(&key);
                    state.reactions.retain(|reaction| match reaction {
                        Reaction::Connected(node, _)
                        | Reaction::Disconnected(node, _)
                        | Reaction::Attribute(node, ..)
                        | Reaction::Adopted(node, ..) => *node != key,
                    });
                    state.failed.insert(key);
                    return Err(OpError::new(
                        "TypeError",
                        "custom element constructor did not return its upgraded element",
                    ));
                }
                if !consumed {
                    self.state.borrow_mut().failed.insert(key);
                    return Err(OpError::new(
                        "NotSupportedError",
                        "custom element constructor did not call super()",
                    ));
                }
                self.mark_upgraded(&realm, id, custom_name);
            }
        }
        Ok(())
    }

    pub(crate) fn deliver_reactions(&self, ctx: &mut Ctx) -> OpResult<()> {
        // Reactions enqueued by callbacks run in the same checkpoint, in FIFO order.
        let mut steps = 0usize;
        let mut first_error = None;
        loop {
            let reaction = self.state.borrow_mut().reactions.pop_front();
            let Some(reaction) = reaction else {
                break;
            };
            steps += 1;
            if steps > 100_000 {
                return Err(OpError::new(
                    "QuotaExceededError",
                    "custom element reaction loop exceeded limit",
                ));
            }
            let (id, callback, args) = match reaction {
                Reaction::Connected(id, callback) | Reaction::Disconnected(id, callback) => {
                    (id, callback, Vec::new())
                }
                Reaction::Attribute(id, callback, name, old, new, namespace) => {
                    let args = vec![
                        Value::from_string(name),
                        old.map(Value::from_string).unwrap_or(Value::Null),
                        new.map(Value::from_string).unwrap_or(Value::Null),
                        namespace.map(Value::from_string).unwrap_or(Value::Null),
                    ];
                    (id, callback, args)
                }
                Reaction::Adopted(id, callback, old_document, new_document) => {
                    (id, callback, vec![old_document, new_document])
                }
            };
            if callback.is_callable() {
                let target = self
                    .realm_for_node(id)
                    .unwrap_or_else(|| self.realm.clone())
                    .wrap(ctx, id.node);
                if let Err(failure) = ctx.invoke(callback, target, &args) {
                    first_error.get_or_insert(failure);
                }
            }
        }
        first_error.map_or(Ok(()), |failure| Err(OpError::thrown(failure)))
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

    pub(crate) fn upgrade_created_element(&self, ctx: &mut Ctx, id: NodeId) -> OpResult<()> {
        self.upgrade_subtree(ctx, &self.realm, id)
    }

    fn consume_pending_upgrade(&self) -> Option<(Rc<DomRealm>, NodeId)> {
        let mut state = self.state.borrow_mut();
        let pending = state.pending_upgrade.last_mut()?;
        if pending.consumed_by_super {
            return None;
        }
        pending.consumed_by_super = true;
        Some((pending.realm.clone(), pending.node))
    }

    pub(crate) fn mark_upgraded(&self, realm: &Rc<DomRealm>, id: NodeId, name: String) {
        let key = RealmNode::new(realm, id);
        let (connected, attributes) = {
            let session = realm.session.borrow();
            let document = session.document();
            let attributes = match document.kind(id) {
                Ok(NodeKind::Element { attributes, .. }) => attributes
                    .iter()
                    .enumerate()
                    .map(|(index, (name, value))| {
                        let namespace = document
                            .attribute_namespace_uri_at(id, index)
                            .map(str::to_owned);
                        let name = if namespace.is_some() {
                            name.as_str().rsplit(':').next().unwrap_or(name.as_str())
                        } else {
                            name.as_str()
                        };
                        (name.to_owned(), value.clone(), namespace)
                    })
                    .collect(),
                _ => Vec::new(),
            };
            (tree_connected(document, id), attributes)
        };
        let mut state = self.state.borrow_mut();
        state.upgraded.insert(key, name.clone());
        state.connected.insert(key, connected);
        let attribute_callback = state.definitions.get(&name).and_then(|definition| {
            definition
                .callbacks
                .get("attributeChangedCallback")
                .cloned()
                .map(|callback| (callback, definition.observed_attributes.clone()))
        });
        if let Some((callback, observed)) = attribute_callback {
            for (attribute, value, namespace) in attributes {
                if observed.contains(&attribute) {
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
        }
        if connected {
            let callback = state
                .definitions
                .get(&name)
                .and_then(|definition| definition.callbacks.get("connectedCallback"))
                .cloned()
                .unwrap_or(Value::Undefined);
            state
                .reactions
                .push_back(Reaction::Connected(key, callback));
        }
        drop(state);
        self.schedule();
    }

    fn schedule(&self) {
        if !self.scheduled.replace(true) {
            if let Some(callback) = self.delivery.borrow().as_ref() {
                self.jobs.borrow_mut().push(callback.clone());
            }
        }
    }
}

/// Put a hub in interpreter state and attach its mutation observer. The caller installs the
/// returned singleton as `globalThis.customElements`.
pub(crate) fn install(ctx: &mut Ctx, realm: Rc<DomRealm>) -> OpResult<()> {
    let hub = CustomElementHub::new(realm, ctx.deferred_microtasks());
    hub.observe_mutations();
    ctx.op_state()
        .put(HubSlot(Rc::new(RefCell::new(Some(hub.clone())))));
    ctx.class_constructor::<ReactionDelivery>();
    let delivery = ctx.new_instance(ReactionDelivery { hub: hub.clone() });
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
    let registry = hub.registry_value(ctx);
    let constructor = ctx.class_constructor::<DomCustomElementRegistry>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "CustomElementRegistry", constructor)
        .and_then(|_| ctx.member_set(&global, "customElements", registry))
        .map_err(|_| OpError::new("Error", "CustomElementRegistry installation failed"))
}

pub(crate) fn hub_from_ctx(ctx: &mut Ctx) -> Option<CustomElementHub> {
    let slot = ctx.host_mut::<HubSlot>()?.0.clone();
    let hub = slot.borrow().clone();
    hub
}

/// Transfer upgraded custom-element state with adopted nodes and queue the standard lifecycle
/// reaction with the old and new owner documents.
pub(crate) fn adopt_nodes(
    ctx: &mut Ctx,
    source: &Rc<DomRealm>,
    target: &Rc<DomRealm>,
    mapping: &[(NodeId, NodeId)],
) -> OpResult<()> {
    let Some(hub) = hub_from_ctx(ctx) else {
        return Ok(());
    };
    if !hub.owns_realm(source) {
        return Ok(());
    }
    hub.observe_mutations_for(target);
    let old_document = source.document_value(ctx);
    let new_document = target.document_value(ctx);
    let mut queued = false;
    {
        let mut state = hub.state.borrow_mut();
        for &(old, new) in mapping {
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
            for reaction in &mut state.reactions {
                match reaction {
                    Reaction::Connected(node, _)
                    | Reaction::Disconnected(node, _)
                    | Reaction::Attribute(node, ..)
                    | Reaction::Adopted(node, ..)
                        if *node == old_key =>
                    {
                        *node = new_key
                    }
                    _ => {}
                }
            }
            if let Some(callback) = definition_name
                .as_ref()
                .and_then(|name| state.definitions.get(name))
                .and_then(|definition| definition.callbacks.get("adoptedCallback"))
                .cloned()
            {
                state.reactions.push_back(Reaction::Adopted(
                    new_key,
                    callback,
                    old_document.clone(),
                    new_document.clone(),
                ));
                queued = true;
            }
        }
    }
    if queued {
        hub.schedule();
    }
    Ok(())
}

pub(crate) fn upgrade_created_element(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    id: NodeId,
) -> OpResult<()> {
    if let Some(hub) = hub_from_ctx(ctx) {
        if Rc::ptr_eq(&hub.realm, realm) {
            hub.upgrade_created_element(ctx, id)?;
        }
    }
    Ok(())
}

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
    construct_for_superclass(ctx, this, Some(expected_tag))
}

fn construct_for_superclass(
    ctx: &mut Ctx,
    this: Value,
    expected_tag: Option<&str>,
) -> OpResult<HtmlElementCtor> {
    let target = ctx.current_new_target();
    let hub = hub_from_ctx(ctx)
        .ok_or_else(|| OpError::new("TypeError", "custom element registry is unavailable"))?;
    let (name, extends) = hub
        .constructor_for_new_target(ctx, &target)
        .ok_or_else(|| OpError::new("TypeError", "Illegal constructor"))?;
    if extends.as_deref() != expected_tag {
        return Err(OpError::new(
            "TypeError",
            "custom element called the wrong HTML superclass constructor",
        ));
    }
    let pending = hub.consume_pending_upgrade();
    let (realm, id, upgrading) = if let Some((realm, id)) = pending {
        (realm, id, true)
    } else {
        let realm = hub.realm.clone();
        let id = realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: extends.as_deref().unwrap_or(&name).into(),
                attributes: extends
                    .as_ref()
                    .map(|_| vec![("is".into(), name.clone())])
                    .unwrap_or_default(),
            })
            .map_err(dom_error)?;
        (realm, id, false)
    };
    if !upgrading {
        hub.mark_upgraded(&realm, id, name.clone());
    }
    Ok(HtmlElementCtor {
        this,
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
        Value::Str(value) => Ok(Some(value.as_str().to_owned())),
        value => {
            let is = ctx.member_get(&value, "is").map_err(OpError::thrown)?;
            if matches!(is, Value::Undefined | Value::Null) {
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
    match tag {
        "input" => Some("HTMLInputElement"),
        "select" => Some("HTMLSelectElement"),
        "option" => Some("HTMLOptionElement"),
        "textarea" => Some("HTMLTextAreaElement"),
        "form" => Some("HTMLFormElement"),
        "style" => Some("HTMLStyleElement"),
        "iframe" => Some("HTMLIFrameElement"),
        "template" => Some("HTMLTemplateElement"),
        _ => None,
    }
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
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || !name.contains('-') {
        return false;
    }
    if !chars.all(|ch| {
        let cp = ch as u32;
        ch.is_ascii_lowercase()
            || ch.is_ascii_digit()
            || matches!(cp,
            0x2D | 0x2E | 0x5F | 0xB7 | 0xC0..=0xD6 | 0xD8..=0xF6 | 0xF8..=0x37D |
            0x37F..=0x1FFF | 0x200C..=0x200D | 0x203F..=0x2040 | 0x2070..=0x218F |
            0x2C00..=0x2FEF | 0x3001..=0xD7FF | 0xF900..=0xFDCF | 0xFDF0..=0xFFFD |
            0x10000..=0xEFFFF)
    }) {
        return false;
    }
    ![
        "annotation-xml",
        "color-profile",
        "font-face",
        "font-face-src",
        "font-face-uri",
        "font-face-format",
        "font-face-name",
        "missing-glyph",
    ]
    .contains(&name)
}

/// Host-specific constructor result used by `HTMLElement`'s native binding. The concrete binding
/// hook is intentionally kept in `lib.rs`; this builder is the point where the engine's
/// superclass-return substitution preserves a wrapper's Node/EventTarget identity during upgrade.
pub(crate) struct HtmlElementCtor {
    pub(crate) this: Value,
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) id: NodeId,
    pub(crate) custom_name: Option<String>,
    pub(crate) interface: Option<String>,
    pub(crate) upgrading: bool,
}

impl HtmlElementCtor {
    fn into_ctor_for(self, ctx: &mut Ctx, expected_interface: &str) -> Result<Value, Value> {
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
        if self.upgrading {
            let wrapper = self.realm.wrap(ctx, self.id);
            let proto = ctx.prototype_of(&self.this);
            let set_proto = set_prototype_function(ctx)?;
            ctx.invoke(set_proto, Value::Undefined, &[wrapper.clone(), proto])?;
            ctx.replace_current_native_super_result(wrapper.clone())?;
        } else {
            let proto = ctx.prototype_of(&self.this);
            let set_proto = set_prototype_function(ctx)?;
            attach_html_native(ctx, &self.this, &self.realm, self.id, expected_interface)?;
            ctx.invoke(set_proto, Value::Undefined, &[self.this.clone(), proto])?;
            let weak = ctx.weak_value(&self.this).ok_or_else(|| {
                ctx.make_error("TypeError", "HTMLElement instance is not an object")
            })?;
            self.realm.wrappers.borrow_mut().insert(self.id, weak);
            ctx.replace_current_native_super_result(self.this.clone())?;
        }
        Ok(self.this)
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
custom_ctor_ret!(DomInputElement, "HTMLInputElement");
custom_ctor_ret!(DomSelectElement, "HTMLSelectElement");
custom_ctor_ret!(DomOptionElement, "HTMLOptionElement");
custom_ctor_ret!(DomTextAreaElement, "HTMLTextAreaElement");
custom_ctor_ret!(DomFormElement, "HTMLFormElement");
custom_ctor_ret!(DomStyleElement, "HTMLStyleElement");
custom_ctor_ret!(DomIFrameElement, "HTMLIFrameElement");
custom_ctor_ret!(DomTemplateElement, "HTMLTemplateElement");

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
    let result = match interface {
        "HTMLElement" => ctx.attach_instance(target, html()),
        "HTMLInputElement" => ctx.attach_instance(target, DomInputElement { base: html() }),
        "HTMLSelectElement" => ctx.attach_instance(target, DomSelectElement { base: html() }),
        "HTMLOptionElement" => ctx.attach_instance(target, DomOptionElement { base: html() }),
        "HTMLTextAreaElement" => ctx.attach_instance(target, DomTextAreaElement { base: html() }),
        "HTMLFormElement" => ctx.attach_instance(target, DomFormElement { base: html() }),
        "HTMLStyleElement" => ctx.attach_instance(target, DomStyleElement { base: html() }),
        "HTMLIFrameElement" => ctx.attach_instance(target, DomIFrameElement { base: html() }),
        "HTMLTemplateElement" => ctx.attach_instance(target, DomTemplateElement { base: html() }),
        _ => return Err(ctx.make_error("TypeError", "unsupported custom element base interface")),
    };
    result.map_err(|error| error.into_error(ctx))
}

fn set_prototype_function(ctx: &mut Ctx) -> Result<Value, Value> {
    let global = ctx.global_this();
    let object = ctx.member_get(&global, "Object")?;
    ctx.member_get(&object, "setPrototypeOf")
}

#[cfg(test)]
mod tests {
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
                "old.setAttribute('value','after'); reactions.length === 3"
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
    fn registry_validates_names_constructors_and_observed_attributes() {
        let mut engine = Engine::new();
        let _realm = super::super::install(engine.ctx(), "", 32).unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                r#"
            class Valid extends HTMLElement {}
            var invalidName = false, invalidBase = false, invalidObserved = false, duplicate = false;
            try { customElements.define('badname', Valid); } catch (error) { invalidName = error.name === 'SyntaxError'; }
            try { customElements.define('x-not-element', class {}); } catch (error) { invalidBase = error.name === 'TypeError'; }
            class BadObserved extends HTMLElement { static get observedAttributes() { return 'value'; } attributeChangedCallback() {} }
            try { customElements.define('x-bad-observed', BadObserved); } catch (error) { invalidObserved = error.name === 'TypeError'; }
            customElements.define('x-valid', Valid);
            try { customElements.define('x-valid', class extends HTMLElement {}); } catch (error) { duplicate = error.name === 'NotSupportedError'; }
            invalidName && invalidBase && invalidObserved && duplicate && customElements.get('x-valid') === Valid
        "#
            ),
            Value::Bool(true)
        ));
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
                "var fresh = document.createElement('input', {is:'x-fancy-input'}); fresh instanceof HTMLInputElement && fresh instanceof FancyInput && fresh.localName === 'input' && fresh.getAttribute('is') === 'x-fancy-input' && fresh.createdByCustomCtor",
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
