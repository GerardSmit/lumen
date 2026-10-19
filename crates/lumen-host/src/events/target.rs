//! Listener storage and the dispatch algorithm shared by every event target.
use super::bindings::{CustomEvent, Event, EventTarget};
use super::*;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// A listener's callback.
#[derive(Clone)]
pub enum Callback {
    /// An event handler whose value is `null` (Node keeps its position) or failed to compile.
    Empty,
    Function(JsFunction),
    /// A callback interface object (`handleEvent`).
    Object(Value),
    /// A listener registered with Node's `kWeakHandler`.
    Weak(WeakValue),
    /// A lazily compiled handler (an HTML content attribute). The source is opaque to the core;
    /// [`TargetHooks::compile_deferred`] compiles it.
    Deferred {
        source: Rc<dyn Any>,
        compiled: Option<JsFunction>,
    },
}

impl Callback {
    fn from_value(value: Value) -> Self {
        match JsFunction::from_value(value.clone()) {
            Some(function) => Self::Function(function),
            None => Self::Object(value),
        }
    }

    /// The value a listener is keyed by (`removeEventListener`, duplicate detection).
    fn identity(&self) -> Option<Value> {
        match self {
            Self::Function(function) => Some(function.value().clone()),
            Self::Object(object) => Some(object.clone()),
            Self::Weak(weak) => weak.upgrade(),
            Self::Empty | Self::Deferred { .. } => None,
        }
    }

    /// The handler value an IDL event handler attribute reports.
    pub fn function(&self) -> Option<JsFunction> {
        match self {
            Self::Function(function)
            | Self::Deferred {
                compiled: Some(function),
                ..
            } => Some(function.clone()),
            _ => None,
        }
    }
}

/// How an event handler listener treats its callback's result and a `null` assignment.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandlerKind {
    /// An `addEventListener` listener.
    None,
    /// An HTML event handler: `false` cancels, `null` removes the listener.
    Html,
    /// Node's `defineEventHandler`: the result is ignored, `null` keeps the listener's position.
    Node,
}

#[derive(Clone)]
pub(crate) struct Listener {
    pub(crate) kind: Rc<str>,
    pub(crate) callback: Rc<RefCell<Callback>>,
    pub(crate) capture: bool,
    pub(crate) once: bool,
    pub(crate) passive: bool,
    pub(crate) handler: HandlerKind,
    pub(crate) node_style: bool,
    pub(crate) resist: bool,
    pub(crate) removed: Rc<Cell<bool>>,
}

impl Listener {
    fn new(kind: &str, callback: Callback, handler: HandlerKind) -> Self {
        Self {
            kind: kind.into(),
            callback: Rc::new(RefCell::new(callback)),
            capture: false,
            once: false,
            passive: false,
            handler,
            node_style: false,
            resist: false,
            removed: Rc::new(Cell::new(false)),
        }
    }

    fn same_callback(&self, callback: &Value) -> bool {
        self.handler == HandlerKind::None
            && self
                .callback
                .borrow()
                .identity()
                .is_some_and(|existing| same(&existing, callback))
    }

    /// A listener that would run: not removed, and not an empty handler or a dead weak callback.
    fn active(&self) -> bool {
        !self.removed.get()
            && match &*self.callback.borrow() {
                Callback::Empty => false,
                Callback::Weak(weak) => weak.upgrade().is_some(),
                _ => true,
            }
    }
}

/// One entry of an event path (DOM "struct" in the event's path).
pub struct PathEntry {
    /// The invocation target (`currentTarget` while its listeners run).
    pub value: Value,
    pub target: Rc<TargetData>,
    /// The shadow-adjusted target seen by listeners of this entry.
    pub adjusted: Value,
    /// Closed shadow scopes that contain this entry (opaque keys, see [`EventState`]).
    pub closed: Vec<u128>,
    /// The shadow-adjusted related target.
    pub related: Value,
}

/// The propagation path of one dispatch. `entries[0]` is the target itself.
pub struct EventPath {
    pub entries: Vec<PathEntry>,
    /// Whether `target` and `relatedTarget` are cleared after dispatch (a target in a shadow
    /// tree).
    pub clear_target: bool,
}

/// The outcome of compiling a [`Callback::Deferred`] handler.
pub enum DeferredCompile {
    Compiled(JsFunction),
    /// Compilation failed and the handler becomes `null`.
    Failed,
    /// The handler stays uncompiled for now.
    Inactive,
}

/// Host extensions of one event target (the DOM tree, a window, a platform object).
///
/// Targets without hooks have a path of their own, no deferred handlers and Node's retention.
pub trait TargetHooks: Any {
    fn as_any(&self) -> &dyn Any;

    /// Notify a platform target before reporting a thrown listener exception.
    /// IndexedDB aborts the associated transaction after the dispatch finishes;
    /// this hook never replaces the realm's normal exception reporting.
    fn listener_exception(&self) {}

    /// Select a platform activation behavior while the dispatch path is stable.
    /// The returned continuation runs after dispatch state has been cleared.
    fn activation_behavior(&self, ctx: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<Option<PreparedActivation>> {
        let _ = (ctx, receiver, event);
        Ok(None)
    }

    /// Apply a platform-specific HTML event-handler return convention.
    /// The shared dispatcher remains independent of DOM event subclasses.
    fn handle_html_return(&self, ctx: &mut Ctx, kind: &str, event: &Value, result: &Value) -> OpResult<bool> {
        let _ = (ctx, kind, event, result);
        Ok(false)
    }

    /// The full propagation path for a dispatch to `receiver`; `None` is the target alone.
    fn event_path(
        &self,
        ctx: &mut Ctx,
        data: &Rc<TargetData>,
        receiver: &Value,
        event: &Event,
        initial_target: &Value,
    ) -> OpResult<Option<EventPath>> {
        let _ = (ctx, data, receiver, event, initial_target);
        Ok(None)
    }

    fn compile_deferred(&self, ctx: &mut Ctx, source: &Rc<dyn Any>) -> DeferredCompile {
        let _ = (ctx, source);
        DeferredCompile::Inactive
    }

    /// Whether `current` is a global object whose `onerror` handler gets the five-argument
    /// special form for an `ErrorEvent`.
    fn is_global_scope(&self, ctx: &mut Ctx, current: &Value) -> bool {
        let _ = (ctx, current);
        false
    }
}

pub trait ActivationBehavior {
    fn pre_activate(&mut self, _ctx: &mut Ctx) -> OpResult<()> { Ok(()) }
    fn finish(self: Box<Self>, ctx: &mut Ctx, accepted: bool) -> OpResult<()>;
}

impl<F: FnOnce(&mut Ctx, bool) -> OpResult<()>> ActivationBehavior for F {
    fn finish(self: Box<Self>, ctx: &mut Ctx, accepted: bool) -> OpResult<()> { (*self)(ctx, accepted) }
}

pub type PreparedActivation = Box<dyn ActivationBehavior>;

/// The native state of an `EventTarget`: its listener list and optional host hooks.
pub struct TargetData {
    pub(crate) listeners: RefCell<Vec<Listener>>,
    hooks: Option<Rc<dyn TargetHooks>>,
    on_change: Cell<Option<ChangeObserver>>,
}

/// Called with the receiver after its listener list changed (see [`TargetData::observe_changes`]).
pub type ChangeObserver = fn(&mut Ctx, &Value, &TargetData);

impl TargetData {
    pub fn new(hooks: Option<Rc<dyn TargetHooks>>) -> Rc<Self> {
        Rc::new(Self {
            listeners: RefCell::new(Vec::new()),
            hooks,
            on_change: Cell::new(None),
        })
    }

    /// Have `observer` run after every change of the listener list (a listener or handler added
    /// or removed). A host class whose lifetime depends on its listeners (`MessagePort`) uses
    /// it to update its own retention.
    pub fn observe_changes(&self, observer: ChangeObserver) {
        self.on_change.set(Some(observer));
    }

    pub fn hooks(&self) -> Option<&Rc<dyn TargetHooks>> {
        self.hooks.as_ref()
    }

    /// The hooks as `H`, when this target has hooks of that type.
    pub fn hooks_as<H: TargetHooks>(&self) -> Option<&H> {
        self.hooks.as_ref()?.as_any().downcast_ref::<H>()
    }

    pub fn has_listeners(&self) -> bool {
        self.listeners.borrow().iter().any(Listener::active)
    }

    /// Whether a live listener's callback is none of `ignored` (an identity check).
    pub fn has_listener_besides(&self, ignored: &[Value]) -> bool {
        self.listeners.borrow().iter().any(|listener| {
            !listener.removed.get()
                && listener
                    .callback
                    .borrow()
                    .identity()
                    .is_some_and(|callback| !ignored.iter().any(|value| same(value, &callback)))
        })
    }

    pub fn is_empty(&self) -> bool {
        self.listeners.borrow().is_empty()
    }

    pub fn listener_count(&self, kind: &str) -> usize {
        self.listeners
            .borrow()
            .iter()
            .filter(|listener| {
                &*listener.kind == kind
                    && !listener.removed.get()
                    && !matches!(&*listener.callback.borrow(), Callback::Empty)
            })
            .count()
    }

    /// The types with at least one listener, in registration order.
    pub fn event_names(&self) -> Vec<Rc<str>> {
        let mut names: Vec<Rc<str>> = Vec::new();
        for listener in self.listeners.borrow().iter() {
            if !listener.removed.get() && !names.iter().any(|name| *name == listener.kind) {
                names.push(listener.kind.clone());
            }
        }
        names
    }

    /// The live callbacks of `kind` (Node's `getEventListeners`).
    pub fn callbacks(&self, kind: &str) -> Vec<Value> {
        self.listeners
            .borrow()
            .iter()
            .filter(|listener| !listener.removed.get() && &*listener.kind == kind)
            .filter_map(|listener| listener.callback.borrow().identity())
            .collect()
    }

    /// Visit each callback Value the list holds (for a traced identity owner).
    pub fn trace_callbacks(&self, visit: &mut dyn FnMut(&Value)) {
        for listener in self.listeners.borrow().iter() {
            match &*listener.callback.borrow() {
                Callback::Function(function)
                | Callback::Deferred {
                    compiled: Some(function),
                    ..
                } => visit(function.value()),
                Callback::Object(object) => visit(object),
                Callback::Empty | Callback::Weak(_) | Callback::Deferred { .. } => {}
            }
        }
    }

    /// Remove every listener (a node leaving its document, a closed window).
    pub fn clear(&self) -> bool {
        let mut listeners = self.listeners.borrow_mut();
        if listeners.is_empty() {
            return false;
        }
        for listener in listeners.iter() {
            listener.removed.set(true);
        }
        listeners.clear();
        true
    }

    /// The callback cell of the `kind` event handler, if one is registered.
    pub fn handler_cell(&self, kind: &str) -> Option<Rc<RefCell<Callback>>> {
        self.listeners
            .borrow()
            .iter()
            .find(|listener| listener.handler != HandlerKind::None && &*listener.kind == kind)
            .map(|listener| listener.callback.clone())
    }

    /// Store `callback` as the `kind` event handler: replace the existing handler in place, or
    /// append a new listener. `None` removes an HTML handler and empties a Node one.
    pub fn set_handler(&self, kind: &str, callback: Option<Callback>, handler: HandlerKind) {
        let mut listeners = self.listeners.borrow_mut();
        let existing = listeners
            .iter()
            .position(|listener| listener.handler != HandlerKind::None && &*listener.kind == kind);
        match (existing, callback) {
            (Some(index), Some(callback)) => *listeners[index].callback.borrow_mut() = callback,
            (None, Some(callback)) => listeners.push(Listener::new(kind, callback, handler)),
            (Some(index), None) if handler == HandlerKind::Node => {
                *listeners[index].callback.borrow_mut() = Callback::Empty;
            }
            (Some(index), None) => {
                listeners[index].removed.set(true);
                listeners.remove(index);
            }
            (None, None) if handler == HandlerKind::Node => {
                listeners.push(Listener::new(kind, Callback::Empty, handler));
            }
            (None, None) => {}
        }
    }

    /// Visit every deferred handler source (HTML rebinds them when a node moves).
    pub fn for_each_deferred(&self, mut visit: impl FnMut(&Rc<dyn Any>) -> Option<Rc<dyn Any>>) {
        for listener in self.listeners.borrow().iter() {
            let mut callback = listener.callback.borrow_mut();
            if let Callback::Deferred { source, .. } = &mut *callback {
                if let Some(replacement) = visit(source) {
                    *source = replacement;
                    // Moving the source's native identity does not change an
                    // already compiled callback or its originating realm.
                }
            }
        }
    }

    fn remove_where(&self, mut matches: impl FnMut(&Listener) -> bool) -> Option<Listener> {
        let mut listeners = self.listeners.borrow_mut();
        let index = listeners.iter().position(|listener| matches(listener))?;
        let listener = listeners.remove(index);
        listener.removed.set(true);
        Some(listener)
    }

    /// Compile a deferred handler through the hooks, caching the result in `cell`.
    pub fn resolve_deferred(&self, ctx: &mut Ctx, cell: &RefCell<Callback>) -> Callback {
        let current = cell.borrow().clone();
        let Callback::Deferred {
            source,
            compiled: None,
        } = &current
        else {
            return current;
        };
        let Some(hooks) = self.hooks.clone() else {
            return current;
        };
        match hooks.compile_deferred(ctx, source) {
            DeferredCompile::Compiled(function) => {
                let resolved = Callback::Deferred {
                    source: source.clone(),
                    compiled: Some(function),
                };
                *cell.borrow_mut() = resolved.clone();
                resolved
            }
            DeferredCompile::Failed => {
                *cell.borrow_mut() = Callback::Empty;
                Callback::Empty
            }
            DeferredCompile::Inactive => current,
        }
    }
}

/// The options of one `addEventListener` call.
#[derive(Default)]
pub struct ListenerOptions {
    pub capture: bool,
    pub once: bool,
    pub passive: bool,
    pub signal: Option<Value>,
    /// Node's `kWeakHandler` owner.
    pub weak: Option<Value>,
    pub node_style: bool,
    pub resist: bool,
}

impl ListenerOptions {
    /// Web IDL `(AddEventListenerOptions or boolean)`, plus Node's private keys.
    pub fn read(ctx: &mut Ctx, options: &Value, add: bool) -> OpResult<Self> {
        let mut result = Self::default();
        match options {
            Value::Undefined | Value::Null => {}
            Value::Obj(_) => {
                result.capture = flag(ctx, options, "capture")?;
                if add {
                    result.once = flag(ctx, options, "once")?;
                    result.passive = flag(ctx, options, "passive")?;
                    let signal = ctx.member_get(options, "signal").map_err(OpError::thrown)?;
                    if !matches!(signal, Value::Undefined) {
                        result.signal = Some(signal);
                    }
                    let weak = symbol_get(ctx, options, WEAK_HANDLER)?;
                    if !matches!(weak, Value::Undefined | Value::Null) {
                        result.weak = Some(weak);
                    }
                    result.resist = matches!(
                        symbol_get(ctx, options, RESIST_STOP_PROPAGATION)?,
                        Value::Bool(true)
                    );
                }
            }
            other => result.capture = ctx.to_boolean(other),
        }
        Ok(result)
    }
}

fn flag(ctx: &mut Ctx, options: &Value, key: &str) -> OpResult<bool> {
    let value = ctx.member_get(options, key).map_err(OpError::thrown)?;
    Ok(ctx.to_boolean(&value))
}

impl EventTarget {
    pub fn from_data(data: Rc<TargetData>) -> Self {
        Self { data }
    }

    pub fn with_hooks(hooks: Rc<dyn TargetHooks>) -> Self {
        Self::from_data(TargetData::new(Some(hooks)))
    }

    pub fn data_handle(&self) -> Rc<TargetData> {
        self.data.clone()
    }

    pub fn data(&self) -> &Rc<TargetData> {
        &self.data
    }

    /// The target behind `receiver`; `undefined` / `null` select the global object when it is
    /// itself an event target (a window).
    pub fn of_receiver(ctx: &mut Ctx, receiver: &Value) -> OpResult<(Rc<TargetData>, Value)> {
        let receiver = match receiver {
            Value::Null | Value::Undefined => ctx.global_object(),
            receiver => receiver.clone(),
        };
        let data = ctx
            .with_instance::<EventTarget, _>(&receiver, |target| target.data.clone())
            .map_err(|_| invalid_this("EventTarget"))?;
        Ok((data, receiver))
    }

    /// Keep the wrapper and its callbacks alive while it has listeners: HTML targets are rooted
    /// as before; other targets trace their callbacks from the wrapper.
    pub fn update_retention(ctx: &mut Ctx, data: &TargetData, receiver: &Value) {
        let listening = !data.is_empty();
        if data.hooks.is_none() && listening {
            let _ = ctx.ensure_native_identity_owner::<EventTarget>(receiver);
        }
        ctx.retain_instance(receiver, listening);
        if let Some(observer) = data.on_change.get() {
            observer(ctx, receiver, data);
        }
    }

    /// `addEventListener` after argument conversion.
    pub fn add_listener(
        ctx: &mut Ctx,
        receiver: &Value,
        data: &Rc<TargetData>,
        kind: &str,
        callback: Value,
        options: ListenerOptions,
    ) -> OpResult<()> {
        if let Some(signal) = &options.signal {
            if super::abort::signal_state(ctx, signal).is_none() {
                return Err(invalid_arg_type(
                    ctx,
                    "options.signal",
                    "an instance of AbortSignal",
                    signal,
                ));
            }
        }
        if !matches!(callback, Value::Obj(_)) {
            if matches!(callback, Value::Null | Value::Undefined) {
                emit_null_listener_warning(ctx, receiver, kind, &callback);
                return Ok(());
            }
            return Err(invalid_arg_type(
                ctx,
                "listener",
                "an instance of EventListener",
                &callback,
            ));
        }
        let signal_state = match &options.signal {
            Some(signal) => {
                let state = super::abort::signal_state(ctx, signal).expect("validated signal");
                if state.aborted.get() {
                    return Ok(());
                }
                Some(state)
            }
            None => None,
        };
        let capture = options.capture;
        {
            let mut listeners = data.listeners.borrow_mut();
            listeners.retain(|listener| {
                let dead = matches!(&*listener.callback.borrow(), Callback::Weak(weak) if weak.upgrade().is_none());
                if dead {
                    listener.removed.set(true);
                }
                !dead
            });
            if listeners.iter().any(|listener| {
                &*listener.kind == kind && listener.capture == capture && listener.same_callback(&callback)
            }) {
                return Ok(());
            }
        }
        let weak = match &options.weak {
            Some(owner) => {
                let held = match ctx.weak_value(&callback) {
                    Some(weak) => weak,
                    None => unreachable!("listener is an object"),
                };
                if matches!(owner, Value::Obj(_)) {
                    let slot = ctx.allocate_native_private_slot_name();
                    let _ = ctx.define_native_private_value_slot(owner, &slot, callback.clone());
                }
                Some(held)
            }
            None => None,
        };
        let is_weak = weak.is_some();
        let listener = Listener {
            kind: kind.into(),
            callback: Rc::new(RefCell::new(match weak {
                Some(weak) => Callback::Weak(weak),
                None => Callback::from_value(callback.clone()),
            })),
            capture,
            once: options.once,
            passive: options.passive,
            handler: HandlerKind::None,
            node_style: options.node_style,
            resist: options.resist,
            removed: Rc::new(Cell::new(false)),
        };
        if let Some(state) = signal_state {
            super::abort::add_listener_removal(ctx, &state, data, receiver, &listener);
        }
        let removed = listener.removed.clone();
        data.listeners.borrow_mut().push(listener);
        Self::update_retention(ctx, data, receiver);
        if !removed.get() {
            let size = data.listener_count(kind);
            new_listener_hook(
                ctx,
                receiver,
                size,
                kind,
                &callback,
                [options.once, capture, options.passive, is_weak],
            )?;
        }
        Ok(())
    }

    /// `removeEventListener` after argument conversion.
    pub fn remove_listener(
        ctx: &mut Ctx,
        receiver: &Value,
        data: &Rc<TargetData>,
        kind: &str,
        callback: &Value,
        capture: bool,
    ) -> OpResult<()> {
        let removed = data.remove_where(|listener| {
            &*listener.kind == kind && listener.capture == capture && listener.same_callback(callback)
        });
        if removed.is_none() {
            return Ok(());
        }
        Self::update_retention(ctx, data, receiver);
        let size = data.listener_count(kind);
        remove_listener_hook(ctx, receiver, size, kind, callback, capture)
    }

    /// Remove the listener whose removal flag is `removed` (an aborted `signal` option).
    pub(crate) fn remove_flagged(
        ctx: &mut Ctx,
        receiver: &Value,
        data: &Rc<TargetData>,
        removed: &Rc<Cell<bool>>,
    ) -> OpResult<()> {
        let Some(listener) = data.remove_where(|listener| Rc::ptr_eq(&listener.removed, removed))
        else {
            return Ok(());
        };
        Self::update_retention(ctx, data, receiver);
        let callback = listener
            .callback
            .borrow()
            .identity()
            .unwrap_or(Value::Undefined);
        let size = data.listener_count(&listener.kind);
        remove_listener_hook(ctx, receiver, size, &listener.kind, &callback, listener.capture)
    }

    /// Remove every listener of `kind` (or all of them).
    pub fn remove_all(ctx: &mut Ctx, receiver: &Value, data: &Rc<TargetData>, kind: Option<&str>) {
        {
            let mut listeners = data.listeners.borrow_mut();
            listeners.retain(|listener| {
                let remove = kind.map_or(true, |kind| &*listener.kind == kind);
                if remove {
                    listener.removed.set(true);
                }
                !remove
            });
        }
        Self::update_retention(ctx, data, receiver);
    }

    /// `dispatchEvent`: a script dispatch keeps the event's own trust (Node's `kTrustEvent`).
    pub fn dispatch(ctx: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<bool> {
        let trusted = ctx
            .with_instance::<Event, _>(event, |event| event.trust_init.get())
            .map_err(|_| invalid_arg_type(ctx, "event", "an instance of Event", event))?;
        dispatch_event(ctx, receiver, event, trusted, None)
    }

    /// Dispatch an event the user agent created (`isTrusted` is true).
    pub fn dispatch_trusted(ctx: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<bool> {
        dispatch_event(ctx, receiver, event, true, None)
    }

    /// [`Self::dispatch_trusted`] with a different initial `target` (HTML's Window lifecycle
    /// events report the Document while the path stays the Window's).
    pub fn dispatch_trusted_with_target(
        ctx: &mut Ctx,
        receiver: &Value,
        event: &Value,
        target: Value,
    ) -> OpResult<bool> {
        dispatch_event(ctx, receiver, event, true, Some(target))
    }

    /// Node's `emit(type, arg)`: Node-style listeners receive `arg`; a `CustomEvent` with
    /// `detail: arg` is created only for the first DOM-style listener. Returns whether there
    /// were listeners.
    pub fn emit(ctx: &mut Ctx, receiver: &Value, kind: &str, arg: Value) -> OpResult<bool> {
        Self::emit_with(ctx, receiver, kind, arg, None)
    }

    /// [`Self::emit`] where a DOM-style listener receives the `Event` returned by `factory`
    /// (called at most once, without arguments) instead of a `CustomEvent`.
    pub fn emit_with(
        ctx: &mut Ctx,
        receiver: &Value,
        kind: &str,
        arg: Value,
        factory: Option<Value>,
    ) -> OpResult<bool> {
        let (data, receiver) = Self::of_receiver(ctx, receiver)?;
        if data.listener_count(kind) == 0 {
            return Ok(false);
        }
        let mut source = EventSource::Lazy {
            kind: kind.into(),
            detail: arg,
            factory,
            created: None,
        };
        let result = invoke(ctx, &mut source, &receiver, &data, false, 2);
        if let EventSource::Lazy {
            created: Some((event, _)),
            ..
        } = &source
        {
            event.dispatching.set(false);
            event.phase.set(0);
            *event.current.borrow_mut() = Value::Null;
        }
        result.map(|()| true)
    }
}

fn emit_null_listener_warning(ctx: &mut Ctx, receiver: &Value, kind: &str, callback: &Value) {
    let Some((process, emit)) = process_emit_warning(ctx) else {
        return;
    };
    let text = if matches!(callback, Value::Null) { "null" } else { "undefined" };
    let warning = ctx.make_error(
        "Error",
        format!("addEventListener called with {text} which has no effect."),
    );
    let _ = ctx.member_set(&warning, "name", Value::str("AddEventListenerArgumentTypeWarning"));
    let _ = ctx.member_set(&warning, "target", receiver.clone());
    let _ = ctx.member_set(&warning, "type", Value::str(kind));
    let _ = ctx.invoke(emit, process, &[warning]);
}

/// `process` and its `emitWarning`, when the realm has Node's process object.
pub(crate) fn process_emit_warning(ctx: &mut Ctx) -> Option<(Value, Value)> {
    let global = ctx.global_object();
    let process = ctx.member_get(&global, "process").ok()?;
    if !matches!(process, Value::Obj(_)) {
        return None;
    }
    let emit = ctx.member_get(&process, "emitWarning").ok()?;
    emit.is_callable().then_some((process, emit))
}

pub(crate) fn new_listener_hook(
    ctx: &mut Ctx,
    receiver: &Value,
    size: usize,
    kind: &str,
    callback: &Value,
    [once, capture, passive, weak]: [bool; 4],
) -> OpResult<()> {
    super::abort::listener_added(ctx, receiver, kind, weak);
    if let Some(symbols) = EventSymbols::existing(ctx) {
        let hook = ctx
            .reflect_get(receiver, &symbols.new_listener, receiver)
            .map_err(OpError::thrown)?;
        if hook.is_callable() {
            ctx.invoke(
                hook,
                receiver.clone(),
                &[
                    Value::Num(size as f64),
                    Value::str(kind),
                    callback.clone(),
                    Value::Bool(once),
                    Value::Bool(capture),
                    Value::Bool(passive),
                    Value::Bool(weak),
                ],
            )
            .map_err(OpError::thrown)?;
            return Ok(());
        }
    }
    max_listeners_warning(ctx, receiver, size, kind)
}

pub(crate) fn remove_listener_hook(
    ctx: &mut Ctx,
    receiver: &Value,
    size: usize,
    kind: &str,
    callback: &Value,
    capture: bool,
) -> OpResult<()> {
    super::abort::listener_removed(ctx, receiver, kind, size);
    let Some(symbols) = EventSymbols::existing(ctx) else {
        return Ok(());
    };
    let hook = ctx
        .reflect_get(receiver, &symbols.remove_listener, receiver)
        .map_err(OpError::thrown)?;
    if hook.is_callable() {
        ctx.invoke(
            hook,
            receiver.clone(),
            &[
                Value::Num(size as f64),
                Value::str(kind),
                callback.clone(),
                Value::Bool(capture),
            ],
        )
        .map_err(OpError::thrown)?;
    }
    Ok(())
}

/// Node's default `kNewListener`: warn once when a type exceeds the target's max listeners.
pub(crate) fn max_listeners_warning(
    ctx: &mut Ctx,
    receiver: &Value,
    size: usize,
    kind: &str,
) -> OpResult<()> {
    let Some((process, emit)) = process_emit_warning(ctx) else {
        return Ok(());
    };
    let max_key = ctx.symbol_for(MAX_LISTENERS);
    let warned_key = ctx.symbol_for(MAX_LISTENERS_WARNED);
    let max = match ctx
        .reflect_get(receiver, &max_key, receiver)
        .map_err(OpError::thrown)?
    {
        Value::Undefined => default_max_listeners(ctx),
        Value::Num(max) => max,
        other => ctx.coerce_number(&other).map_err(OpError::thrown)?,
    };
    if !(max > 0.0 && size as f64 > max) {
        return Ok(());
    }
    let warned = ctx
        .reflect_get(receiver, &warned_key, receiver)
        .map_err(OpError::thrown)?;
    if ctx.to_boolean(&warned) {
        return Ok(());
    }
    set_data_property(ctx, receiver, warned_key, Value::Bool(true))?;
    let global = ctx.global_object();
    let inspect_key = ctx.symbol_for("lumen.inspect");
    let inspect = ctx
        .reflect_get(&global, &inspect_key, &global)
        .map_err(OpError::thrown)?;
    let shown = if inspect.is_callable() {
        let options = ctx.new_object_with_proto(&Value::Null);
        ctx.member_set(&options, "depth", Value::Num(-1.0))
            .map_err(OpError::thrown)?;
        let shown = ctx
            .invoke(inspect, Value::Undefined, &[receiver.clone(), options])
            .map_err(OpError::thrown)?;
        ctx.coerce_string(&shown).map_err(OpError::thrown)?.to_string()
    } else {
        constructor_name(ctx, receiver).unwrap_or_else(|| "EventTarget".into())
    };
    let warning = ctx.make_error(
        "Error",
        format!(
            "Possible EventTarget memory leak detected. {size} {kind} listeners added to {shown}. Use events.setMaxListeners() to increase limit"
        ),
    );
    for (key, value) in [
        ("name", Value::str("MaxListenersExceededWarning")),
        ("target", receiver.clone()),
        ("type", Value::str(kind)),
        ("count", Value::Num(size as f64)),
    ] {
        ctx.member_set(&warning, key, value).map_err(OpError::thrown)?;
    }
    ctx.invoke(emit, process, &[warning])
        .map_err(OpError::thrown)?;
    Ok(())
}

/// `EventEmitter.defaultMaxListeners` when Node's `events` is loaded, else 10.
pub(crate) fn default_max_listeners(ctx: &mut Ctx) -> f64 {
    let global = ctx.global_object();
    let key = ctx.symbol_for("lumen.EventEmitter");
    let emitter = ctx
        .reflect_get(&global, &key, &global)
        .unwrap_or(Value::Undefined);
    if !matches!(emitter, Value::Obj(_)) {
        return 10.0;
    }
    match ctx.member_get(&emitter, "defaultMaxListeners") {
        Ok(Value::Num(max)) => max,
        _ => 10.0,
    }
}

pub(crate) fn constructor_name(ctx: &mut Ctx, value: &Value) -> Option<String> {
    let constructor = ctx.member_get(value, "constructor").ok()?;
    if !matches!(constructor, Value::Obj(_)) {
        return None;
    }
    let name = ctx.member_get(&constructor, "name").ok()?;
    ctx.coerce_string(&name).ok().map(|name| name.to_string())
}

/// The event a dispatch delivers: an existing `Event`, or Node `emit`'s lazily created one.
enum EventSource {
    Existing { event: Event, value: Value },
    Lazy {
        kind: Rc<str>,
        detail: Value,
        factory: Option<Value>,
        created: Option<(Event, Value)>,
    },
}

impl EventSource {
    fn event(&self) -> Option<&Event> {
        match self {
            Self::Existing { event, .. } => Some(event),
            Self::Lazy { created, .. } => created.as_ref().map(|(event, _)| event),
        }
    }

    fn kind(&self) -> Rc<str> {
        match self {
            Self::Existing { event, .. } => event.kind.borrow().as_str().into(),
            Self::Lazy { kind, .. } => kind.clone(),
        }
    }

    /// The event value for a DOM-style listener, creating `emit`'s `CustomEvent` once.
    fn value(&mut self, ctx: &mut Ctx, current: &Value, phase: u8) -> OpResult<Value> {
        match self {
            Self::Existing { value, .. } => Ok(value.clone()),
            Self::Lazy {
                kind,
                detail,
                factory,
                created,
            } => {
                if created.is_none() {
                    let value = match factory {
                        Some(factory) => ctx
                            .invoke(factory.clone(), Value::Undefined, &[])
                            .map_err(OpError::thrown)?,
                        None => CustomEvent::create(ctx, kind, detail.clone())?,
                    };
                    let event = ctx
                        .with_instance::<Event, _>(&value, Clone::clone)
                        .map_err(|_| invalid_arg_type(ctx, "event", "an instance of Event", &value))?;
                    ctx.set_native_identity_owner::<Event>(&value)?;
                    *event.target.borrow_mut() = current.clone();
                    event.dispatching.set(true);
                    *created = Some((event, value));
                }
                let (event, value) = created.as_ref().expect("created above");
                *event.current.borrow_mut() = current.clone();
                event.phase.set(phase);
                Ok(value.clone())
            }
        }
    }

    fn node_value(&self) -> Option<Value> {
        match self {
            Self::Existing { .. } => None,
            Self::Lazy { detail, .. } => Some(detail.clone()),
        }
    }
}

fn dispatch_event(
    ctx: &mut Ctx,
    receiver: &Value,
    event_value: &Value,
    trusted: bool,
    target_override: Option<Value>,
) -> OpResult<bool> {
    let (data, receiver) = EventTarget::of_receiver(ctx, receiver)?;
    let event = ctx
        .with_instance::<Event, _>(event_value, Clone::clone)
        .map_err(|_| invalid_arg_type(ctx, "event", "an instance of Event", event_value))?;
    if event.dispatching.get() || !event.initialized.get() {
        return Err(recursion_error(ctx, &event));
    }
    // Native dispatch state owns observable target Values after dispatch. Make them edges of
    // this event wrapper rather than opaque external GC roots.
    ctx.ensure_native_identity_owner::<Event>(event_value)?;
    event.dispatching.set(true);
    event.trusted.set(trusted);
    event.stopped.set(false);
    event.immediate.set(false);
    let initial_target = target_override.unwrap_or_else(|| receiver.clone());
    *event.target.borrow_mut() = initial_target.clone();
    let mut source = EventSource::Existing {
        event: event.clone(),
        value: event_value.clone(),
    };
    let mut activation = None;
    let result = (|| {
        let path = match data.hooks.clone() {
            Some(hooks) => hooks.event_path(ctx, &data, &receiver, &event, &initial_target)?,
            None => None,
        };
        let path = match path {
            Some(path) => path,
            None => EventPath {
                entries: vec![PathEntry {
                    value: receiver.clone(),
                    target: data.clone(),
                    adjusted: initial_target.clone(),
                    closed: Vec::new(),
                    related: event.related_original.borrow().clone(),
                }],
                clear_target: false,
            },
        };
        for (index, entry) in path.entries.iter().enumerate() {
            // A retargeted shadow boundary can supply an activation target
            // even when the event does not bubble (DOM dispatch step 6.9.8).
            if index != 0 && !event.bubbles.get() && !same(&entry.value, &entry.adjusted) { continue; }
            if let Some(hooks) = entry.target.hooks() {
                activation = hooks.activation_behavior(ctx, &entry.value, event_value)?;
                if activation.is_some() { break; }
            }
        }
        *event.path.borrow_mut() = path
            .entries
            .iter()
            .map(|entry| (entry.value.clone(), entry.closed.clone()))
            .collect();
        if let Some(activation) = activation.as_mut() { activation.pre_activate(ctx)?; }
        let enter = |event: &Event, entry: &PathEntry| {
            *event.target.borrow_mut() = entry.adjusted.clone();
            *event.related.borrow_mut() = entry.related.clone();
            *event.visibility.borrow_mut() = entry.closed.clone();
        };
        for entry in path.entries.iter().skip(1).rev() {
            enter(&event, entry);
            let phase = if same(&entry.value, &entry.adjusted) { 2 } else { 1 };
            invoke(ctx, &mut source, &entry.value, &entry.target, true, phase)?;
            if event.stopped.get() {
                break;
            }
        }
        let own = &path.entries[0];
        if !event.stopped.get() {
            *event.target.borrow_mut() = own.adjusted.clone();
            *event.related.borrow_mut() = own.related.clone();
            *event.visibility.borrow_mut() = own.closed.clone();
            invoke(ctx, &mut source, &own.value, &own.target, true, 2)?;
            if !event.immediate.get() {
                invoke(ctx, &mut source, &own.value, &own.target, false, 2)?;
            }
        }
        if !event.stopped.get() {
            for entry in path.entries.iter().skip(1) {
                if !event.bubbles.get() && !same(&entry.value, &entry.adjusted) {
                    continue;
                }
                enter(&event, entry);
                let phase = if same(&entry.value, &entry.adjusted) { 2 } else { 3 };
                invoke(ctx, &mut source, &entry.value, &entry.target, false, phase)?;
                if event.stopped.get() {
                    break;
                }
            }
        }
        let last = path.entries.last().expect("an event path has its target");
        let (target, related) = if path.clear_target {
            (Value::Null, Value::Null)
        } else {
            (last.adjusted.clone(), last.related.clone())
        };
        *event.target.borrow_mut() = target;
        *event.related.borrow_mut() = related;
        Ok(!event.canceled.get())
    })();
    event.dispatching.set(false);
    event.phase.set(0);
    event.stopped.set(false);
    event.immediate.set(false);
    *event.current.borrow_mut() = Value::Null;
    event.path.borrow_mut().clear();
    event.visibility.borrow_mut().clear();
    if let Some(activation) = activation {
        let accepted = result.as_ref().is_ok_and(|accepted| *accepted);
        if let Err(error) = activation.finish(ctx, accepted) {
            if result.is_ok() { return Err(error); }
        }
    }
    result
}

fn recursion_error(ctx: &mut Ctx, event: &Event) -> OpError {
    if RealmPolicy::current(ctx).is_some_and(|policy| policy.dom_errors) {
        return OpError::new(
            "InvalidStateError",
            "The event is not initialized or is already being dispatched",
        );
    }
    if !event.initialized.get() {
        return OpError::new("InvalidStateError", "The event is not initialized");
    }
    OpError::error(format!(
        "The event \"{}\" is already being dispatched",
        event.kind.borrow()
    ))
    .with_code("ERR_EVENT_RECURSION")
}

/// Run the listeners of one target for one phase.
fn invoke(
    ctx: &mut Ctx,
    source: &mut EventSource,
    current: &Value,
    data: &Rc<TargetData>,
    capture: bool,
    phase: u8,
) -> OpResult<()> {
    let kind = source.kind();
    let listeners: Vec<Listener> = {
        let all = data.listeners.borrow();
        if all.is_empty() {
            return Ok(());
        }
        all.iter()
            .filter(|listener| {
                listener.capture == capture && !listener.removed.get() && listener.kind == kind
            })
            .cloned()
            .collect()
    };
    if listeners.is_empty() {
        return Ok(());
    }
    if let Some(event) = source.event() {
        event.phase.set(phase);
        *event.current.borrow_mut() = current.clone();
    }
    for listener in listeners {
        if source.event().is_some_and(|event| event.immediate.get()) && !listener.resist {
            break;
        }
        if listener.removed.get() {
            continue;
        }
        let callback = match data.resolve_deferred(ctx, &listener.callback) {
            Callback::Weak(weak) => match weak.upgrade() {
                Some(value) => Callback::from_value(value),
                None => {
                    data.remove_where(|entry| Rc::ptr_eq(&entry.removed, &listener.removed));
                    continue;
                }
            },
            Callback::Empty | Callback::Deferred { compiled: None, .. } => continue,
            Callback::Deferred {
                compiled: Some(function),
                ..
            } => Callback::Function(function),
            callback => callback,
        };
        if listener.once {
            data.remove_where(|entry| Rc::ptr_eq(&entry.removed, &listener.removed));
            EventTarget::update_retention(ctx, data, current);
            let identity = callback.identity().unwrap_or(Value::Undefined);
            let size = data.listener_count(&kind);
            remove_listener_hook(ctx, current, size, &kind, &identity, listener.capture)?;
        }
        let result = call_listener(ctx, source, data, &listener, &callback, current, phase);
        if let Some(event) = source.event() {
            event.passive.set(false);
        }
        match result {
            Ok(Some(value)) => {
                if listener.handler == HandlerKind::None {
                    report_rejection(ctx, &value);
                }
            }
            Ok(None) => {}
            Err(error) => {
                if let Some(hooks) = &data.hooks { hooks.listener_exception(); }
                // An ancestor listener can also throw while processing a
                // platform operation rooted at the original event target.
                if let Some(event) = source.event() {
                    let original = event.target.borrow().clone();
                    if let Ok((original_data, _)) = EventTarget::of_receiver(ctx, &original) {
                        if !Rc::ptr_eq(data, &original_data) {
                            if let Some(hooks) = &original_data.hooks { hooks.listener_exception(); }
                        }
                    }
                }
                let exception = error.to_value(ctx);
                report_exception(ctx, exception);
            }
        }
    }
    Ok(())
}

/// Call one listener; returns its result when it may be a promise to report.
fn call_listener(
    ctx: &mut Ctx,
    source: &mut EventSource,
    data: &Rc<TargetData>,
    listener: &Listener,
    callback: &Callback,
    current: &Value,
    phase: u8,
) -> OpResult<Option<Value>> {
    if listener.node_style {
        if let Some(value) = source.node_value() {
            let Callback::Function(function) = callback else {
                return Ok(None);
            };
            return function.call(ctx, current.clone(), &[value]).map(Some);
        }
    }
    let event_value = source.value(ctx, current, phase)?;
    let event = source.event().expect("an event exists once its value does").clone();
    event.passive.set(listener.passive);
    if listener.handler != HandlerKind::None {
        let Some(function) = callback.function() else {
            return Ok(None);
        };
        let special = &*listener.kind == "error"
            && listener.handler == HandlerKind::Html
            && data
                .hooks
                .clone()
                .is_some_and(|hooks| hooks.is_global_scope(ctx, current));
        let special_args = if special {
            super::bindings::ErrorEvent::handler_arguments(ctx, &event_value)
        } else {
            None
        };
        if let Some(args) = special_args {
            let result = function.call(ctx, current.clone(), &args)?;
            if matches!(result, Value::Bool(true)) {
                event.prevent_default();
            }
            return Ok(None);
        }
        let result = function.call(ctx, current.clone(), &[event_value.clone()])?;
        if listener.handler == HandlerKind::Html {
            if let Some(hooks) = data.hooks.clone() {
                if hooks.handle_html_return(ctx, &listener.kind, &event_value, &result)? {
                    return Ok(None);
                }
            }
        }
        if listener.handler == HandlerKind::Html && matches!(result, Value::Bool(false)) {
            event.prevent_default();
        }
        return Ok(None);
    }
    match callback {
        Callback::Function(function) => function
            .call(ctx, current.clone(), &[event_value])
            .map(Some),
        Callback::Object(object) => {
            let handle_event = ctx
                .member_get(object, "handleEvent")
                .map_err(OpError::thrown)?;
            let Some(handle_event) = JsFunction::from_value(handle_event) else {
                return Err(OpError::type_error("EventListener.handleEvent is not callable"));
            };
            handle_event
                .call(ctx, object.clone(), &[event_value])
                .map(Some)
        }
        _ => Ok(None),
    }
}

/// A promise returned by a listener reports its rejection like a thrown exception (Node), unless
/// the realm reports exceptions itself (HTML ignores listener results).
fn report_rejection(ctx: &mut Ctx, value: &Value) {
    if !matches!(value, Value::Obj(_)) || RealmPolicy::current(ctx).is_some() {
        return;
    }
    let Ok(then) = ctx.member_get(value, "then") else {
        return;
    };
    if !then.is_callable() {
        return;
    }
    let report = ctx.new_native_fn(
        "",
        1,
        Rc::new(|ctx: &mut Ctx, _this: Value, args: &[Value]| {
            report_exception(ctx, args.first().cloned().unwrap_or(Value::Undefined));
            Ok(Value::Undefined)
        }),
    );
    let _ = ctx.invoke(then, value.clone(), &[Value::Undefined, report]);
}

/// Report an exception thrown by a listener: the realm's reporter (HTML's `error` event), or
/// Node's behavior of rethrowing it on a later turn.
pub fn report_exception(ctx: &mut Ctx, exception: Value) {
    if let Some(policy) = RealmPolicy::current(ctx) {
        (policy.report)(ctx, exception);
        return;
    }
    let thrown = exception.clone();
    let rethrow = ctx.new_native_fn(
        "",
        0,
        Rc::new(move |_: &mut Ctx, _this: Value, _: &[Value]| Err(thrown.clone())),
    );
    let global = ctx.global_object();
    if let Some((process, next_tick)) = ctx
        .member_get(&global, "process")
        .ok()
        .filter(|process| matches!(process, Value::Obj(_)))
        .and_then(|process| {
            let next_tick = ctx.member_get(&process, "nextTick").ok()?;
            next_tick.is_callable().then_some((process, next_tick))
        })
    {
        if ctx.invoke(next_tick, process, &[rethrow.clone()]).is_ok() {
            return;
        }
    }
    if let Ok(set_timeout) = ctx.member_get(&global, "setTimeout") {
        if set_timeout.is_callable()
            && ctx
                .invoke(set_timeout, Value::Undefined, &[rethrow.clone(), Value::Num(0.0)])
                .is_ok()
        {
            return;
        }
    }
    ctx.queue_microtask(rethrow);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AliasPath(RefCell<Option<WeakValue>>);
    impl TargetHooks for AliasPath {
        fn as_any(&self) -> &dyn Any { self }
        fn event_path(&self,_:&mut Ctx,data:&Rc<TargetData>,_:&Value,event:&Event,_:&Value) -> OpResult<Option<EventPath>> {
            let alias=self.0.borrow().as_ref().and_then(WeakValue::upgrade)
                .expect("published platform alias");
            Ok(Some(EventPath { entries:vec![PathEntry {
                value:alias.clone(),target:data.clone(),adjusted:alias,
                closed:Vec::new(),related:event.related_original(),
            }],clear_target:false }))
        }
    }

    #[test]
    fn specification_window_platform_path_alias_controls_at_target_identity_and_callback_receiver() {
        let mut engine=lumen::Engine::new();
        crate::globals::<super::super::bindings::Module>(engine.ctx()).ok().expect("event globals");
        let hooks=Rc::new(AliasPath(RefCell::new(None)));
        let data=TargetData::new(Some(hooks.clone()));
        let source=engine.ctx().new_instance(EventTarget::from_data(data.clone()));
        let alias=engine.ctx().new_instance(EventTarget::from_data(data));
        *hooks.0.borrow_mut()=engine.ctx().weak_value(&alias);
        let global=engine.ctx().global_this_value();
        engine.ctx().set_member(&global,"source",source).ok().expect("source target");
        engine.ctx().set_member(&global,"alias",alias).ok().expect("published alias");
        let result=engine.eval_value(r#"(() => {
            const calls=[];
            const listener=function(event){calls.push([this===alias,event.target===alias,event.currentTarget===alias,event.eventPhase]);};
            source.addEventListener('probe',listener,true);
            source.addEventListener('probe',listener,false);
            const event=new Event('probe');
            const accepted=source.dispatchEvent(event);
            return accepted && JSON.stringify(calls)==='[[true,true,true,2],[true,true,true,2]]' && event.target===alias && event.currentTarget===null;
        })()"#).expect("script parses").ok().expect("platform alias dispatch");
        assert!(matches!(result,Value::Bool(true)));
    }

    struct PhaseHooks;
    struct PhaseAction { receiver: Value, event: Value }
    impl ActivationBehavior for PhaseAction {
        fn pre_activate(&mut self, ctx: &mut Ctx) -> OpResult<()> {
            let path = ctx.with_instance::<Event, _>(&self.event,
                |event| event.path.borrow().len())?;
            if path == 0 { return Err(OpError::type_error("preactivation must see the constructed event path")); }
            ctx.member_set(&self.receiver, "preactivated", Value::Bool(true)).map_err(OpError::thrown)?;
            Ok(())
        }
        fn finish(self: Box<Self>, ctx: &mut Ctx, accepted: bool) -> OpResult<()> {
            let clean = ctx.with_instance::<Event, _>(&self.event,
                |event| !event.dispatching.get() && matches!(*event.current.borrow(), Value::Null) && event.path.borrow().is_empty())?;
            ctx.member_set(&self.receiver, "finished", Value::Bool(clean)).map_err(OpError::thrown)?;
            ctx.member_set(&self.receiver, "accepted", Value::Bool(accepted)).map_err(OpError::thrown)?;
            Ok(())
        }
    }
    impl TargetHooks for PhaseHooks {
        fn as_any(&self) -> &dyn Any { self }
        fn activation_behavior(&self, _: &mut Ctx, receiver: &Value, event: &Value) -> OpResult<Option<PreparedActivation>> {
            Ok(Some(Box::new(PhaseAction { receiver: receiver.clone(), event: event.clone() })))
        }
    }
    #[test]
    fn specification_window_shared_activation_phases_surround_dispatch_and_reset_event_state() {
        let mut engine=lumen::Engine::new();
        crate::globals::<super::super::bindings::Module>(engine.ctx()).ok().expect("event globals");
        let target=engine.ctx().new_instance(EventTarget::from_data(TargetData::new(Some(Rc::new(PhaseHooks)))));
        let global=engine.ctx().global_this_value();
        engine.ctx().set_member(&global,"target",target).ok().expect("activation target");
        let result=engine.eval_value(r#"(() => {
            target.addEventListener('click',event=>{
                if(!target.preactivated || target.finished)throw new Error('activation ordering');
                event.preventDefault();
            });
            const event=new Event('click',{cancelable:true});
            return !target.dispatchEvent(event) && target.finished && target.accepted===false;
        })()"#).unwrap().ok().expect("shared activation script");
        assert!(matches!(result,Value::Bool(true)));
    }
}
