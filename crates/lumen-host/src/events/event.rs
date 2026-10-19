//! `Event` state and the operations host subclasses share.
use super::bindings::Event;
use super::*;
use std::cell::{Cell, RefCell};

/// The state of one event. A cloned [`Event`] handle shares it, so a subclass's embedded base and
/// the dispatcher see the same flags.
pub struct EventState {
    pub(crate) kind: RefCell<String>,
    pub(crate) time_stamp: f64,
    pub(crate) bubbles: Cell<bool>,
    pub(crate) cancelable: Cell<bool>,
    pub(crate) composed: Cell<bool>,
    pub(crate) initialized: Cell<bool>,
    pub(crate) trusted: Cell<bool>,
    /// Trust requested at construction (Node's `kTrustEvent`), kept by script dispatch.
    pub(crate) trust_init: Cell<bool>,
    /// The path of the current dispatch, with each entry's closed shadow scopes.
    pub(crate) path: RefCell<Vec<(Value, Vec<u128>)>>,
    /// The closed shadow scopes visible from the current invocation target.
    pub(crate) visibility: RefCell<Vec<u128>>,
    pub(crate) related_original: RefCell<Value>,
    pub(crate) related: RefCell<Value>,
    pub(crate) target: RefCell<Value>,
    pub(crate) current: RefCell<Value>,
    pub(crate) phase: Cell<u8>,
    pub(crate) dispatching: Cell<bool>,
    pub(crate) stopped: Cell<bool>,
    pub(crate) immediate: Cell<bool>,
    pub(crate) canceled: Cell<bool>,
    pub(crate) passive: Cell<bool>,
    /// UI Events' `movementX` / `movementY`, set by the user agent on pointer events.
    movement: Cell<(f64, f64)>,
}

/// The `EventInit` members every event reads.
#[derive(Clone, Copy, Default)]
pub struct EventInit {
    pub bubbles: bool,
    pub cancelable: bool,
    pub composed: bool,
    pub trusted: bool,
}

impl EventInit {
    /// Read an `EventInit` dictionary (`undefined` and `null` are empty).
    pub fn read(ctx: &mut Ctx, options: &Option<Value>) -> OpResult<Self> {
        let options = match options {
            None | Some(Value::Undefined | Value::Null) => return Ok(Self::default()),
            Some(options @ Value::Obj(_)) => options,
            Some(other) => {
                return Err(invalid_arg_type(ctx, "options", "of type object", other));
            }
        };
        let mut flag = |key| -> OpResult<bool> {
            let value = ctx.member_get(options, key).map_err(OpError::thrown)?;
            Ok(ctx.to_boolean(&value))
        };
        let bubbles = flag("bubbles")?;
        let cancelable = flag("cancelable")?;
        let composed = flag("composed")?;
        let trusted = match EventSymbols::existing(ctx) {
            Some(symbols) => matches!(
                ctx.reflect_get(options, &symbols.trust, options)
                    .map_err(OpError::thrown)?,
                Value::Bool(true)
            ),
            None => false,
        };
        Ok(Self {
            bubbles,
            cancelable,
            composed,
            trusted,
        })
    }
}

impl std::ops::Deref for Event {
    type Target = EventState;

    fn deref(&self) -> &EventState {
        &self.state
    }
}

impl lumen::embed::NativeIdentityOwner for Event {
    const TRACES_NATIVE_VALUES: bool = true;

    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        visit(&self.target.borrow());
        visit(&self.current.borrow());
        visit(&self.related_original.borrow());
        visit(&self.related.borrow());
        for (target, _) in self.path.borrow().iter() {
            visit(target);
        }
    }
}

impl Event {
    /// A new event of `kind` (the constructor after argument conversion).
    pub fn from_init(kind: &str, init: EventInit) -> Self {
        Self {
            state: Rc::new(EventState {
                kind: RefCell::new(kind.into()),
                time_stamp: crate::perf::web_now_ms(),
                bubbles: Cell::new(init.bubbles),
                cancelable: Cell::new(init.cancelable),
                composed: Cell::new(init.composed),
                initialized: Cell::new(true),
                trusted: Cell::new(init.trusted),
                trust_init: Cell::new(init.trusted),
                path: RefCell::new(Vec::new()),
                visibility: RefCell::new(Vec::new()),
                related_original: RefCell::new(Value::Null),
                related: RefCell::new(Value::Null),
                target: RefCell::new(Value::Null),
                current: RefCell::new(Value::Null),
                phase: Cell::new(0),
                dispatching: Cell::new(false),
                stopped: Cell::new(false),
                immediate: Cell::new(false),
                canceled: Cell::new(false),
                passive: Cell::new(false),
                movement: Cell::new((0.0, 0.0)),
            }),
        }
    }

    /// `new Event(kind, options)` from Rust.
    pub fn new(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
        Ok(Self::from_init(kind, EventInit::read(ctx, &options)?))
    }

    /// A trusted event the user agent fires (`AbortSignal`'s `abort`).
    pub fn trusted(kind: &str) -> Self {
        Self::from_init(
            kind,
            EventInit {
                trusted: true,
                ..EventInit::default()
            },
        )
    }

    pub fn kind(&self) -> String {
        self.kind.borrow().clone()
    }

    pub fn trusted_dispatch(&self) -> bool { self.trusted.get() }

    pub fn prevent_default(&self) {
        if self.cancelable.get() && !self.passive.get() {
            self.canceled.set(true);
        }
    }

    pub fn is_dispatching(&self) -> bool {
        self.dispatching.get()
    }

    pub fn default_prevented(&self) -> bool {
        self.canceled.get()
    }

    /// The target while the event is being dispatched.
    pub fn active_dispatch_target(&self) -> Option<Value> {
        self.dispatching.get().then(|| self.target.borrow().clone())
    }

    /// The last retargeted event target, which remains on the event after dispatch. Derived
    /// attributes whose values are nodes apply the same shadow-boundary adjustment as `target`.
    pub fn target_for_retarget(&self) -> Value {
        self.target.borrow().clone()
    }

    pub fn active_current_target(&self) -> Option<Value> {
        self.dispatching
            .get()
            .then(|| self.current.borrow().clone())
    }

    /// The common legacy initialization steps (`initEvent`). Derived initializers call this
    /// before updating their own fields; it does nothing while the event is dispatched.
    pub fn initialize_legacy(&self, kind: &str, bubbles: bool, cancelable: bool) -> bool {
        if self.dispatching.get() {
            return false;
        }
        self.initialized.set(true);
        self.stopped.set(false);
        self.immediate.set(false);
        self.canceled.set(false);
        self.trusted.set(false);
        *self.target.borrow_mut() = Value::Null;
        *self.kind.borrow_mut() = kind.into();
        self.bubbles.set(bubbles);
        self.cancelable.set(cancelable);
        true
    }

    /// An event `document.createEvent` returns: not initialized until `initEvent`.
    pub fn legacy_uninitialized() -> Self {
        let event = Self::from_init("", EventInit::default());
        event.mark_uninitialized();
        event
    }

    /// Reset every flag and clear the initialized flag.
    pub fn mark_uninitialized(&self) {
        self.initialized.set(false);
        *self.kind.borrow_mut() = String::new();
        self.bubbles.set(false);
        self.cancelable.set(false);
        self.composed.set(false);
        self.trusted.set(false);
        self.trust_init.set(false);
        *self.target.borrow_mut() = Value::Null;
        *self.current.borrow_mut() = Value::Null;
        *self.related_original.borrow_mut() = Value::Null;
        *self.related.borrow_mut() = Value::Null;
        self.phase.set(0);
        self.dispatching.set(false);
        self.stopped.set(false);
        self.immediate.set(false);
        self.canceled.set(false);
        self.passive.set(false);
        self.path.borrow_mut().clear();
        self.visibility.borrow_mut().clear();
        self.movement.set((0.0, 0.0));
    }

    pub fn set_movement(&self, x: f64, y: f64) {
        self.movement.set((x, y));
    }

    pub fn movement(&self) -> (f64, f64) {
        self.movement.get()
    }

    /// The related target the dispatcher retargets along the path (`MouseEvent`, `FocusEvent`).
    pub fn set_related_target(&self, value: Value) {
        *self.related_original.borrow_mut() = value.clone();
        *self.related.borrow_mut() = value;
    }

    pub fn related_target(&self) -> Value {
        self.related.borrow().clone()
    }

    pub fn related_original(&self) -> Value {
        self.related_original.borrow().clone()
    }

    pub fn composed_flag(&self) -> bool {
        self.composed.get()
    }
}
