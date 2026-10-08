//! Owned host-created ECMAScript realms and scoped native access to them.
//!
//! This module deliberately builds on the interpreter's existing realm registry and snapshot
//! operations. It does not create a second interpreter or a Node `vm` sandbox: callers receive
//! the actual child global and can install native host objects while that realm is active.

use crate::fasthash::FastMap;
use crate::interpreter::{Abrupt, Interp, RealmState};
use crate::value::{Gc, Value};
use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

/// Opaque handle to a realm created by [`Interp::create_host_realm`].
///
/// The returned global is the realm's actual global object, not a contextified sandbox proxy.
/// Handles are tied to the interpreter which created them and are intentionally not `Send`.
#[derive(Clone)]
pub struct RealmHandle {
    owner: Rc<()>,
    global: Value,
    key: usize,
}

impl RealmHandle {
    /// Clone the actual global object for installing native host state or passing to
    /// [`Interp::eval_in_realm`].
    pub fn global(&self) -> Value {
        self.global.clone()
    }

    /// Whether two retained handles identify the same realm in the same interpreter.
    ///
    /// This intentionally exposes identity comparison without exposing the interpreter's
    /// internal realm key. Host registries can use it to associate pending work with a realm
    /// and cancel that work when a browsing context is retired.
    pub fn same_realm(&self, other: &Self) -> bool {
        self.key == other.key && Rc::ptr_eq(&self.owner, &other.owner)
    }

    pub(crate) fn key(&self) -> usize {
        self.key
    }

    pub(crate) fn belongs_to(&self, interp: &Interp) -> bool {
        interp
            .host_state
            .get::<HostRealmOwner>()
            .is_some_and(|owner| Rc::ptr_eq(&owner.token, &self.owner))
    }

    fn is_registered_in(&self, interp: &Interp) -> bool {
        self.belongs_to(interp)
            && (Gc::as_ptr(&interp.global) as usize == self.key
                || interp.realms.contains_key(&self.key))
    }
}

/// A typed request made by a native `WindowProxy` internal method.
///
/// The `receiver` fields are passed through unchanged, including receivers supplied by explicit
/// `Reflect.get` / `Reflect.set` calls. A policy runs before the engine reads any target property.
#[derive(Clone)]
pub enum WindowProxyOperation {
    Get {
        key: Value,
        receiver: Value,
    },
    Set {
        key: Value,
        value: Value,
        receiver: Value,
    },
    Has {
        key: Value,
    },
    Delete {
        key: Value,
    },
    GetOwnProperty {
        key: Value,
    },
    DefineOwnProperty {
        key: Value,
        descriptor: Value,
    },
    OwnPropertyKeys,
    GetPrototypeOf,
    SetPrototypeOf {
        prototype: Value,
    },
    IsExtensible,
    PreventExtensions,
}

/// A result that is valid only for the corresponding `WindowProxyOperation` variant.
///
/// This prevents a host policy from accidentally returning, for example, an own-property
/// descriptor as the result of `[[Get]]` or a key list as the result of `[[Has]]`.
#[derive(Clone)]
pub enum WindowProxyResult {
    Get(Value),
    Set(bool),
    Has(bool),
    Delete(bool),
    GetOwnProperty(Option<Value>),
    DefineOwnProperty(bool),
    OwnPropertyKeys(Vec<Value>),
    GetPrototypeOf(Value),
    SetPrototypeOf(bool),
    IsExtensible(bool),
    PreventExtensions(bool),
}

/// The outcome of a host authorization check for one WindowProxy operation.
#[derive(Clone)]
pub enum WindowProxyDisposition {
    /// The caller and current target are same-origin; the engine performs the ordinary operation.
    ForwardSameOrigin,
    /// A trusted host handled this exact operation, typically for a cross-origin allowlisted key.
    Handled(WindowProxyResult),
    /// Deny the operation with the host-created exception value.
    Denied(Value),
}

/// Host authorization and browsing-context child lookup for a WindowProxy.
///
/// Implementations must not retain JS `Value`s or realm globals in the policy object. The engine
/// treats the wrapped global as one explicit internal GC edge and cannot trace arbitrary values
/// captured in native Rust state. Host code should keep lifecycle state outside this object and
/// return child proxies only from the short-lived lookup callbacks.
pub trait WindowProxyPolicy {
    /// Authorize or handle an operation before the engine touches the wrapped Window's data.
    fn decide(
        &self,
        ctx: &mut Interp,
        caller: &RealmHandle,
        target: &RealmHandle,
        operation: &WindowProxyOperation,
    ) -> WindowProxyDisposition;

    /// Authorize a later native `Window` receiver check, such as invoking a same-origin Window
    /// method previously read through this proxy. The default denies access. This check cannot
    /// create a DOMException because it is used by native brand validation; script-visible
    /// cross-origin operations must be rejected earlier by [`Self::decide`].
    fn authorize_native_window_receiver(
        &self,
        ctx: &mut Interp,
        _caller: &RealmHandle,
        _target: &RealmHandle,
    ) -> Result<(), Value> {
        Err(ctx.make_error("TypeError", "Illegal invocation"))
    }

    /// Number of document-tree child navigables exposed by an authorized same-origin target.
    fn child_window_count(
        &self,
        _ctx: &mut Interp,
        _caller: &RealmHandle,
        _target: &RealmHandle,
    ) -> usize {
        0
    }

    /// Return the current WindowProxy at an authorized child index, without constructing an index
    /// map. The engine verifies that the value is itself a registered WindowProxy in this engine.
    fn child_window_at(
        &self,
        _ctx: &mut Interp,
        _caller: &RealmHandle,
        _target: &RealmHandle,
        _index: u32,
    ) -> Option<Value> {
        None
    }
}

#[derive(Clone)]
pub(crate) struct WindowProxyEntry {
    pub(crate) target: RealmHandle,
    pub(crate) policy: Rc<dyn WindowProxyPolicy>,
}

pub(crate) struct WindowProxyRegistry {
    pub(crate) entries: FastMap<usize, WindowProxyEntry>,
    active_receivers: Rc<RefCell<Vec<(usize, usize, usize)>>>,
}

impl Default for WindowProxyRegistry {
    fn default() -> Self {
        Self {
            entries: FastMap::default(),
            active_receivers: Rc::new(RefCell::new(Vec::new())),
        }
    }
}

pub(crate) struct WindowProxyForward {
    pub(crate) target: Value,
    pub(crate) target_realm: RealmHandle,
    caller: RealmHandle,
    policy: Rc<dyn WindowProxyPolicy>,
}

pub(crate) enum WindowProxyDecision {
    Forward(WindowProxyForward),
    Handled(WindowProxyResult),
}

impl WindowProxyResult {
    fn matches_operation(&self, operation: &WindowProxyOperation) -> bool {
        matches!(
            (self, operation),
            (Self::Get(_), WindowProxyOperation::Get { .. })
                | (Self::Set(_), WindowProxyOperation::Set { .. })
                | (Self::Has(_), WindowProxyOperation::Has { .. })
                | (Self::Delete(_), WindowProxyOperation::Delete { .. })
                | (
                    Self::GetOwnProperty(_),
                    WindowProxyOperation::GetOwnProperty { .. }
                )
                | (
                    Self::DefineOwnProperty(_),
                    WindowProxyOperation::DefineOwnProperty { .. }
                )
                | (
                    Self::OwnPropertyKeys(_),
                    WindowProxyOperation::OwnPropertyKeys
                )
                | (
                    Self::GetPrototypeOf(_),
                    WindowProxyOperation::GetPrototypeOf
                )
                | (
                    Self::SetPrototypeOf(_),
                    WindowProxyOperation::SetPrototypeOf { .. }
                )
                | (Self::IsExtensible(_), WindowProxyOperation::IsExtensible)
                | (
                    Self::PreventExtensions(_),
                    WindowProxyOperation::PreventExtensions
                )
        )
    }
}

/// Failure to enter an opaque host realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRealmScopeError {
    /// The handle belongs to a different `Interp` instance.
    DifferentInterpreter,
    /// The handle's global is no longer present in this interpreter's realm table.
    UnknownRealm,
}

impl fmt::Display for HostRealmScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentInterpreter => f.write_str("realm belongs to a different interpreter"),
            Self::UnknownRealm => f.write_str("host realm is not registered in this interpreter"),
        }
    }
}

impl std::error::Error for HostRealmScopeError {}

/// Failure to parse or enter a host-realm script.
#[derive(Debug)]
pub enum HostRealmEvalError {
    /// The script could not be parsed or its precompiled syntax snapshot was invalid.
    Parse(crate::ParseError),
    /// The realm handle does not belong to, or is no longer registered in, this engine.
    Scope(HostRealmScopeError),
}

impl fmt::Display for HostRealmEvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "{}", error.message),
            Self::Scope(error) => fmt::Display::fmt(error, f),
        }
    }
}

impl std::error::Error for HostRealmEvalError {}

/// Failure to install a host-published global-this value on a registered realm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostGlobalThisError {
    DifferentInterpreter,
    UnknownRealm,
    NotObject,
    NotGlobalOrWindowProxy,
    WindowProxyTargetsDifferentRealm,
    MissingGlobalThisBinding,
}

impl fmt::Display for HostGlobalThisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentInterpreter => f.write_str("realm belongs to a different interpreter"),
            Self::UnknownRealm => f.write_str("host realm is not registered in this interpreter"),
            Self::NotObject => f.write_str("global-this value must be an object"),
            Self::NotGlobalOrWindowProxy => f.write_str(
                "global-this value must be the realm global or a registered WindowProxy",
            ),
            Self::WindowProxyTargetsDifferentRealm => {
                f.write_str("WindowProxy does not currently target this realm")
            }
            Self::MissingGlobalThisBinding => {
                f.write_str("realm globalThis property or global this binding is unavailable")
            }
        }
    }
}

impl std::error::Error for HostGlobalThisError {}

/// Failure to retire a host-created realm for cycle collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostRealmDisposeError {
    DifferentInterpreter,
    UnknownRealm,
    NotHostManaged,
    ActiveRealm,
}

impl fmt::Display for HostRealmDisposeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentInterpreter => f.write_str("realm belongs to a different interpreter"),
            Self::UnknownRealm => f.write_str("host realm is not registered in this interpreter"),
            Self::NotHostManaged => f.write_str("realm was not created by the host realm API"),
            Self::ActiveRealm => f.write_str("the active realm cannot be retired"),
        }
    }
}

impl std::error::Error for HostRealmDisposeError {}

/// Failure to create or retarget a native WindowProxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowProxyError {
    DifferentInterpreter,
    UnknownRealm,
    NotWindowProxy,
}

impl fmt::Display for WindowProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DifferentInterpreter => {
                f.write_str("WindowProxy target belongs to a different interpreter")
            }
            Self::UnknownRealm => f.write_str("WindowProxy target realm is not registered"),
            Self::NotWindowProxy => f.write_str("value is not a native WindowProxy"),
        }
    }
}

impl std::error::Error for WindowProxyError {}

/// The per-interpreter identity used to reject handles from another engine.
///
/// `OpState` is owned by one `Interp`, while the `Rc` identity cannot collide through allocator
/// address reuse as long as either the interpreter or a handle remains alive.
struct HostRealmOwner {
    token: Rc<()>,
    /// The realm at each JS call site, retained while a cross-realm callee runs with its own
    /// intrinsics active. Native Window receiver checks use the top entry instead of mistaking
    /// the callee's function realm for the caller.
    invocation_callers: Rc<RefCell<Vec<RealmHandle>>>,
    temporary_scopes: Rc<std::cell::Cell<usize>>,
}

fn owner_token(interp: &mut Interp) -> Rc<()> {
    if let Some(owner) = interp.host_state.get::<HostRealmOwner>() {
        return owner.token.clone();
    }
    let owner = Rc::new(());
    interp.host_state.put(HostRealmOwner {
        token: owner.clone(),
        invocation_callers: Rc::new(RefCell::new(Vec::new())),
        temporary_scopes: Rc::new(std::cell::Cell::new(0)),
    });
    owner
}

/// Retain the current call-site realm before a cross-realm callee changes the active globals.
#[cfg(feature = "embed")]
pub(crate) struct HostInvocationGuard {
    callers: Rc<RefCell<Vec<RealmHandle>>>,
}

#[cfg(feature = "embed")]
impl Drop for HostInvocationGuard {
    fn drop(&mut self) {
        self.callers
            .borrow_mut()
            .pop()
            .expect("host invocation caller scopes are balanced");
    }
}

impl Interp {
    /// Install a host-managed global-this value in a known realm.
    ///
    /// The backing global continues to serve global declaration/name lookup. `value` must be
    /// either that realm's global object or a registered WindowProxy whose current target is the
    /// realm. This API intentionally does not accept author-created JavaScript proxies.
    pub fn set_host_global_this(
        &mut self,
        realm: &RealmHandle,
        value: Value,
    ) -> Result<(), HostGlobalThisError> {
        if !realm.belongs_to(self) {
            return Err(HostGlobalThisError::DifferentInterpreter);
        }
        if !realm.is_registered_in(self) {
            return Err(HostGlobalThisError::UnknownRealm);
        }
        let Some(object) = value.as_obj() else {
            return Err(HostGlobalThisError::NotObject);
        };
        let key = Gc::as_ptr(object) as usize;
        if key != realm.key {
            let Some(entry) = self
                .host_state
                .get::<WindowProxyRegistry>()
                .and_then(|registry| registry.entries.get(&key))
            else {
                return Err(HostGlobalThisError::NotGlobalOrWindowProxy);
            };
            if !entry.target.belongs_to(self) {
                return Err(HostGlobalThisError::DifferentInterpreter);
            }
            if !entry.target.is_registered_in(self) {
                return Err(HostGlobalThisError::UnknownRealm);
            }
            if entry.target.key != realm.key {
                return Err(HostGlobalThisError::WindowProxyTargetsDifferentRealm);
            }
        }

        self.with_host_realm(realm, |ctx| ctx.install_global_this_value(value))
            .map_err(|error| match error {
                HostRealmScopeError::DifferentInterpreter => {
                    HostGlobalThisError::DifferentInterpreter
                }
                HostRealmScopeError::UnknownRealm => HostGlobalThisError::UnknownRealm,
            })?
    }

    fn install_global_this_value(&mut self, value: Value) -> Result<(), HostGlobalThisError> {
        if !self.global_env.borrow().vars.contains_key("this") {
            return Err(HostGlobalThisError::MissingGlobalThisBinding);
        }
        {
            let mut global = self.global.borrow_mut();
            let Some(property) = global.props.get_mut("globalThis") else {
                return Err(HostGlobalThisError::MissingGlobalThisBinding);
            };
            if property.accessor() {
                return Err(HostGlobalThisError::MissingGlobalThisBinding);
            }
            property.set_value(value.clone());
        }
        let mut global_env = self.global_env.borrow_mut();
        let binding = global_env
            .vars
            .get_mut("this")
            .expect("global this binding was validated above");
        binding.value = value.clone();
        self.global_this = value;
        Ok(())
    }

    fn active_realm_handle(&mut self) -> RealmHandle {
        let owner = owner_token(self);
        let global = self.global_object();
        let key = global
            .as_obj()
            .map(|object| Gc::as_ptr(object) as usize)
            .expect("active realm has an object global");
        RealmHandle { owner, global, key }
    }

    /// Push the true realm at a JS call site. The callee may run with another realm's
    /// intrinsics, but its native receiver checks must still authorize against this caller.
    #[cfg(feature = "embed")]
    pub(crate) fn enter_host_invocation(&mut self) -> HostInvocationGuard {
        let caller = self.active_realm_handle();
        let callers = self
            .host_state
            .get::<HostRealmOwner>()
            .expect("active realm handle initializes owner state")
            .invocation_callers
            .clone();
        callers.borrow_mut().push(caller);
        HostInvocationGuard { callers }
    }

    fn invocation_caller_handle(&mut self) -> RealmHandle {
        let caller = self
            .host_state
            .get::<HostRealmOwner>()
            .and_then(|owner| owner.invocation_callers.borrow().last().cloned());
        caller.unwrap_or_else(|| self.active_realm_handle())
    }

    /// Return the realm that initiated the current native call, before a cross-realm native
    /// function's own realm was entered. Frame and Window host getters use this for origin checks.
    /// When no native invocation scope is active, the current realm is the caller.
    pub fn invocation_host_realm(&mut self) -> RealmHandle {
        self.invocation_caller_handle()
    }

    /// Retain the current realm for later host callbacks, including the initial realm.
    pub fn current_host_realm(&mut self) -> RealmHandle {
        let owner = owner_token(self);
        let global = self.global_object();
        let key = global
            .as_obj()
            .map(|object| Gc::as_ptr(object) as usize)
            .expect("active realm has an object global");
        let mut snapshot = self.snapshot_realm();
        if let Some(previous) = self.realms.get(&key) {
            snapshot.collectable = previous.collectable;
            snapshot.host_managed = previous.host_managed;
        }
        self.realms.insert(key, snapshot);
        RealmHandle { owner, global, key }
    }

    /// Recover a registered realm handle from an internal global-object identity. Hosts use
    /// this for asynchronous records, such as Promise rejection notifications, whose originating
    /// realm is no longer the currently active interpreter realm.
    pub fn host_realm_for_key(&mut self, key: usize) -> Option<RealmHandle> {
        let active_key = Gc::as_ptr(&self.global) as usize;
        if active_key == key {
            return Some(self.current_host_realm());
        }
        let global = Value::Obj(self.realms.get(&key)?.global.clone());
        Some(RealmHandle {
            owner: owner_token(self),
            global,
            key,
        })
    }

    /// The registered realm whose global object is `global`, if any.
    pub fn host_realm_for_global(&mut self, global: &Value) -> Option<RealmHandle> {
        let key = Gc::as_ptr(global.as_obj()?) as usize;
        self.host_realm_for_key(key)
    }

    /// Resolve a callback's ECMAScript realm using GetFunctionRealm, including
    /// bound functions and callable proxies. Revoked proxies throw their native
    /// TypeError. Retired realms remain available while their functions are live.
    pub fn function_host_realm(
        &mut self,
        function: &crate::embed::JsFunction,
    ) -> Result<RealmHandle, Value> {
        let object = function.value().as_obj().ok_or_else(|| {
            self.make_error("TypeError", "callback must be a function object")
        })?;
        match self.get_function_realm_global(object)
            .map_err(crate::interpreter::abrupt_value)?
        {
            None => Ok(self.current_host_realm()),
            Some(key) => self.host_realm_for_key(key).ok_or_else(|| {
                self.make_error("Error", "callback realm is no longer available")
            }),
        }
    }

    /// Create and register a fresh native-host realm in this interpreter.
    ///
    /// This uses the same realm initialization and intrinsic installation path as Lumen's
    /// existing VM/Realm facilities. The current realm is restored before this method returns.
    pub fn create_host_realm(&mut self) -> RealmHandle {
        let owner = owner_token(self);
        let global = self.create_realm();
        let key = global
            .as_obj()
            .map(|object| Gc::as_ptr(object) as usize)
            .expect("Interp::create_realm returns an object global");
        if let Some(state) = self.realms.get_mut(&key) {
            state.collectable = true;
            state.host_managed = true;
        }
        RealmHandle { owner, global, key }
    }

    /// Mark an inactive host-created realm as eligible for cycle collection.
    ///
    /// This does not discard JavaScript state. The realm remains available to retained functions
    /// and alive while its global, global environment, or a live WindowProxy is reachable. The
    /// caller should drop its host `RealmHandle`s after retiring the browsing context.
    pub fn dispose_host_realm(&mut self, realm: &RealmHandle) -> Result<(), HostRealmDisposeError> {
        if !realm.belongs_to(self) {
            return Err(HostRealmDisposeError::DifferentInterpreter);
        }
        if !realm.is_registered_in(self) {
            return Err(HostRealmDisposeError::UnknownRealm);
        }
        if Gc::as_ptr(&self.global) as usize == realm.key {
            return Err(HostRealmDisposeError::ActiveRealm);
        }
        let state = self
            .realms
            .get(&realm.key)
            .ok_or(HostRealmDisposeError::UnknownRealm)?;
        if !state.host_managed {
            return Err(HostRealmDisposeError::NotHostManaged);
        }
        self.realms
            .get_mut(&realm.key)
            .expect("registered host realm remains present")
            .collectable = true;
        Ok(())
    }

    /// Replace the interpreter's root between host turns. Existing functions
    /// and retained objects still keep their original realm alive; the old root
    /// becomes eligible for the same retirement path as other host realms.
    pub fn replace_root_host_realm(&mut self, realm: &RealmHandle) -> Result<RealmHandle, HostRealmDisposeError> {
        if !realm.belongs_to(self) { return Err(HostRealmDisposeError::DifferentInterpreter); }
        if !realm.is_registered_in(self) { return Err(HostRealmDisposeError::UnknownRealm); }
        let owner = self.host_state.get::<HostRealmOwner>().expect("registered host realm owner");
        if owner.temporary_scopes.get() != 0 || !owner.invocation_callers.borrow().is_empty()
            || !self.fn_frames.is_empty() || self.jit_frames != 0 || self.native_top != 0 || self.depth != 0 {
            return Err(HostRealmDisposeError::ActiveRealm);
        }
        let previous = self.active_realm_handle();
        if previous.same_realm(realm) { return Ok(previous); }
        let mut target = self.realms.get(&realm.key).ok_or(HostRealmDisposeError::UnknownRealm)?.snapshot_clone();
        if !target.host_managed { return Err(HostRealmDisposeError::NotHostManaged); }
        let mut old = self.snapshot_realm();
        old.host_managed = true;
        old.collectable = true;
        target.host_managed = false;
        target.collectable = false;
        self.realms.insert(previous.key, old);
        self.realms.insert(realm.key, target.snapshot_clone());
        self.restore_realm(&target);
        Ok(previous)
    }

    /// Run a native host callback with `realm`'s intrinsics and global installed as the
    /// active interpreter realm.
    ///
    /// The callback's result is returned unchanged. In particular, `Result` values such as JS
    /// throws and `OpResult` errors remain the callback's responsibility. The scope is restored
    /// on every return path and during unwinding. Mutations to the child's active realm state are
    /// snapshotted back into the interpreter's realm table before the caller realm is restored.
    /// Scopes may be nested.
    pub fn with_host_realm<R>(
        &mut self,
        realm: &RealmHandle,
        callback: impl FnOnce(&mut Interp) -> R,
    ) -> Result<R, HostRealmScopeError> {
        let belongs_here = self
            .host_state
            .get::<HostRealmOwner>()
            .is_some_and(|owner| Rc::ptr_eq(&owner.token, &realm.owner));
        if !belongs_here {
            return Err(HostRealmScopeError::DifferentInterpreter);
        }

        let current_key = Gc::as_ptr(&self.global) as usize;
        // The registry snapshot can lag behind an actively executing realm. Re-entering it
        // must keep its live intrinsics, bindings, and native state rather than restore that
        // older snapshot. The outer scope persists it when it eventually exits.
        if current_key == realm.key {
            return Ok(callback(self));
        }
        if self.realms.contains_key(&current_key) {
            let mut snapshot = self.snapshot_realm();
            if let Some(previous) = self.realms.get(&current_key) {
                snapshot.collectable = previous.collectable;
                snapshot.host_managed = previous.host_managed;
            }
            self.realms.insert(current_key, snapshot);
        }

        let target = self
            .realms
            .get(&realm.key)
            .map(RealmState::snapshot_clone)
            .ok_or(HostRealmScopeError::UnknownRealm)?;

        let caller = self.snapshot_realm();
        self.restore_realm(&target);
        let scopes = self.host_state.get::<HostRealmOwner>().expect("host realm owner").temporary_scopes.clone();
        scopes.set(scopes.get() + 1);
        let restore = RestoreRealmOnDrop {
            interp: self as *mut Interp,
            active_key: realm.key,
            caller: Some(caller),
            scopes,
        };

        // `restore` contains a raw pointer so it does not borrow `self` while the callback runs.
        // It cannot escape this function; the exclusive borrow held by this call keeps the
        // interpreter alive and unmoved through the callback and the guard's drop.
        let result = callback(self);
        drop(restore);
        Ok(result)
    }

    /// Temporarily run host bootstrap work in the interpreter tier and restore the prior tier on
    /// return or unwinding. Runtime glue is short-lived setup code; compiling each realm's copy
    /// would repeat work without helping the long-lived installed functions.
    #[doc(hidden)]
    pub fn with_host_bootstrap_tier<R>(&mut self, callback: impl FnOnce(&mut Interp) -> R) -> R {
        let previous = std::mem::replace(&mut self.tier, crate::bytecode::Tier::Interp);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(self)));
        self.tier = previous;
        match result {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Parse and execute a classic host bootstrap Script in a registered realm. Script lexical
    /// declarations persist in that realm, and this method never runs a microtask checkpoint.
    pub fn eval_value_in_host_realm(
        &mut self,
        realm: &RealmHandle,
        source: &str,
        strict: bool,
    ) -> Result<Result<Value, Value>, HostRealmEvalError> {
        self.eval_value_in_host_realm_named(realm, source, strict, None)
    }

    /// Parse and execute a classic Script in a registered host realm, naming it for stack traces.
    /// Script lexical declarations persist in that realm, and this method never runs a microtask
    /// checkpoint.
    pub fn eval_value_in_host_realm_named(
        &mut self,
        realm: &RealmHandle,
        source: &str,
        strict: bool,
        source_name: Option<&str>,
    ) -> Result<Result<Value, Value>, HostRealmEvalError> {
        if crate::native_ops::dynamic_code_disabled() {
            return Err(HostRealmEvalError::Parse(crate::ParseError {
                message: "dynamic code is unavailable in native execution".into(),
                line: 0,
                at_eof: false,
            }));
        }
        let body = crate::parser::parse_script(source, strict).map_err(|error| {
            HostRealmEvalError::Parse(crate::ParseError {
                message: error.message,
                line: error.line,
                at_eof: error.at_eof,
            })
        })?;
        self.run_host_script_body(realm, body, strict, source_name)
    }

    /// Decode and execute a classic host bootstrap Script snapshot without draining jobs. The
    /// shared source remains alive for lazily decoded function bodies in every realm using it.
    pub fn eval_snapshot_shared_source_in_host_realm(
        &mut self,
        realm: &RealmHandle,
        bytes: &[u8],
        source: Rc<str>,
        strict: bool,
    ) -> Result<Result<Value, Value>, HostRealmEvalError> {
        if crate::native_ops::dynamic_code_disabled() {
            return Err(HostRealmEvalError::Parse(crate::ParseError {
                message: "source snapshots are unavailable in the Aot profile".into(),
                line: 0,
                at_eof: false,
            }));
        }
        let body = crate::snapshot::decode_shared(bytes, source).map_err(|message| {
            HostRealmEvalError::Parse(crate::ParseError {
                message,
                line: 0,
                at_eof: false,
            })
        })?;
        self.run_host_script_body(realm, body, strict, None)
    }

    fn run_host_script_body(
        &mut self,
        realm: &RealmHandle,
        body: Vec<crate::ast::Stmt>,
        strict: bool,
        source_name: Option<&str>,
    ) -> Result<Result<Value, Value>, HostRealmEvalError> {
        let directive_strict = matches!(
            body.first(),
            Some(crate::ast::Stmt::Expr(crate::ast::Expr::Str(value))) if &**value == "use strict"
        );
        self.with_host_realm(realm, |ctx| {
            let checkpoint = HostScriptExecutionCheckpoint::capture(ctx);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut scope = HostScriptStrictScope::enter(ctx, strict || directive_strict);
                scope
                    .ctx()
                    .run_program_named(&body, source_name)
                    .map(|value| match value {
                        Value::Empty => Value::Undefined,
                        value => value,
                    })
            }));
            match result {
                Ok(result) => result,
                Err(payload) => {
                    checkpoint.restore(ctx);
                    std::panic::resume_unwind(payload)
                }
            }
        })
        .map_err(HostRealmEvalError::Scope)
    }

    /// Load statically linked native extension glue in a target host realm.
    #[cfg(feature = "aot-native")]
    #[doc(hidden)]
    pub fn load_native_glue_value_in_host_realm(
        &mut self,
        realm: &RealmHandle,
        bytes: &'static [u8],
    ) -> Result<Result<Value, String>, HostRealmScopeError> {
        self.with_host_realm(realm, |ctx| {
            crate::native_aot::load_static_glue_engine(ctx, bytes)
        })
    }

    /// Create a native stable-identity WindowProxy wrapping `target`.
    ///
    /// The proxy's property operations are not exposed to author JavaScript as a general Proxy.
    /// Every operation first calls `policy`; absent a same-origin forwarding decision or a
    /// correctly typed handled result, the policy must return the host's exception value. The
    /// policy must not retain JavaScript `Value`s (see [`WindowProxyPolicy`]).
    #[doc(hidden)]
    pub fn create_window_proxy(
        &mut self,
        target: &RealmHandle,
        policy: Rc<dyn WindowProxyPolicy>,
    ) -> Result<Value, WindowProxyError> {
        self.validate_window_proxy_target(target)?;
        let object = self.new_object();
        // The policy implements exotic internal methods even though this
        // holder uses ordinary object storage. Compiled property operations
        // must forward through the policy rather than cache holder slots.
        object.borrow().ic_plain.set(false);
        let key = Gc::as_ptr(&object) as usize;
        self.ensure_window_proxy_registry();
        self.host_state
            .get_mut::<WindowProxyRegistry>()
            .expect("WindowProxy registry was installed")
            .entries
            .insert(
                key,
                WindowProxyEntry {
                    target: target.clone(),
                    policy,
                },
            );
        self.gc_pin(&object);
        Ok(Value::Obj(object))
    }

    /// Retarget an existing WindowProxy while preserving its JavaScript object identity.
    #[doc(hidden)]
    pub fn retarget_window_proxy(
        &mut self,
        proxy: &Value,
        target: &RealmHandle,
    ) -> Result<(), WindowProxyError> {
        self.validate_window_proxy_target(target)?;
        let key = proxy
            .as_obj()
            .map(|object| Gc::as_ptr(object) as usize)
            .ok_or(WindowProxyError::NotWindowProxy)?;
        let entry = self
            .host_state
            .get_mut::<WindowProxyRegistry>()
            .and_then(|registry| registry.entries.get_mut(&key))
            .ok_or(WindowProxyError::NotWindowProxy)?;
        entry.target = target.clone();
        Ok(())
    }

    fn validate_window_proxy_target(&self, target: &RealmHandle) -> Result<(), WindowProxyError> {
        if !target.belongs_to(self) {
            return Err(WindowProxyError::DifferentInterpreter);
        }
        if !target.is_registered_in(self) {
            return Err(WindowProxyError::UnknownRealm);
        }
        Ok(())
    }

    fn ensure_window_proxy_registry(&mut self) {
        if !self.host_state.has::<WindowProxyRegistry>() {
            self.host_state.put(WindowProxyRegistry::default());
        }
    }

    pub(crate) fn is_window_proxy(&self, object: &Gc) -> bool {
        self.host_state
            .get::<WindowProxyRegistry>()
            .is_some_and(|registry| {
                registry
                    .entries
                    .contains_key(&(Gc::as_ptr(object) as usize))
            })
    }

    /// Avoid call-site realm bookkeeping in embedders that have not created a WindowProxy.
    pub(crate) fn has_window_proxies(&self) -> bool {
        self.host_state
            .get::<WindowProxyRegistry>()
            .is_some_and(|registry| !registry.entries.is_empty())
    }

    pub(crate) fn window_proxy_target(&self, key: usize) -> Option<Value> {
        self.host_state
            .get::<WindowProxyRegistry>()?
            .entries
            .get(&key)
            .map(|entry| entry.target.global())
    }

    pub(crate) fn window_proxy_decision(
        &mut self,
        object: &Gc,
        operation: WindowProxyOperation,
    ) -> Result<Option<WindowProxyDecision>, Abrupt> {
        let key = Gc::as_ptr(object) as usize;
        let Some(entry) = self
            .host_state
            .get::<WindowProxyRegistry>()
            .and_then(|registry| registry.entries.get(&key))
            .cloned()
        else {
            return Ok(None);
        };

        if !entry.target.is_registered_in(self) {
            return Err(self.throw("TypeError", "WindowProxy target realm is no longer valid"));
        }
        let caller = self.active_realm_handle();
        let caller_key = caller.key;
        let disposition = entry
            .policy
            .decide(self, &caller, &entry.target, &operation);
        if Gc::as_ptr(&self.global) as usize != caller_key {
            return Err(self.throw(
                "TypeError",
                "WindowProxy policy did not restore the caller realm",
            ));
        }
        match disposition {
            WindowProxyDisposition::Denied(error) => Err(Abrupt::Throw(error)),
            WindowProxyDisposition::Handled(result) => {
                if !result.matches_operation(&operation) {
                    return Err(self.throw(
                        "TypeError",
                        "WindowProxy policy returned a result for the wrong operation",
                    ));
                }
                Ok(Some(WindowProxyDecision::Handled(result)))
            }
            WindowProxyDisposition::ForwardSameOrigin => {
                if !entry.target.is_registered_in(self) {
                    return Err(self.throw(
                        "TypeError",
                        "WindowProxy target realm changed during authorization",
                    ));
                }
                Ok(Some(WindowProxyDecision::Forward(WindowProxyForward {
                    target: entry.target.global(),
                    target_realm: entry.target,
                    caller,
                    policy: entry.policy,
                })))
            }
        }
    }

    pub(crate) fn window_proxy_child_count(&mut self, forward: &WindowProxyForward) -> usize {
        forward
            .policy
            .child_window_count(self, &forward.caller, &forward.target_realm)
    }

    pub(crate) fn window_proxy_child_at(
        &mut self,
        forward: &WindowProxyForward,
        index: u32,
    ) -> Result<Option<Value>, Abrupt> {
        let child =
            forward
                .policy
                .child_window_at(self, &forward.caller, &forward.target_realm, index);
        let Some(child) = child else {
            return Ok(None);
        };
        let Some(object) = child.as_obj() else {
            return Err(self.throw(
                "TypeError",
                "WindowProxy child lookup returned a non-object",
            ));
        };
        let key = Gc::as_ptr(object) as usize;
        let valid = self
            .host_state
            .get::<WindowProxyRegistry>()
            .and_then(|registry| registry.entries.get(&key))
            .is_some_and(|entry| entry.target.belongs_to(self));
        if !valid {
            return Err(self.throw(
                "TypeError",
                "WindowProxy child lookup returned an unregistered proxy",
            ));
        }
        Ok(Some(child))
    }

    pub(crate) fn with_window_proxy_receiver<R>(
        &mut self,
        proxy_key: usize,
        caller_key: usize,
        target_key: usize,
        callback: impl FnOnce(&mut Interp) -> R,
    ) -> R {
        let active = self
            .host_state
            .get::<WindowProxyRegistry>()
            .map(|registry| registry.active_receivers.clone());
        let Some(active) = active else {
            return callback(self);
        };
        active
            .borrow_mut()
            .push((proxy_key, caller_key, target_key));
        let guard = WindowProxyReceiverGuard { active };
        let result = callback(self);
        drop(guard);
        result
    }
}

/// Restores a realm's prior strict-mode state if host-script evaluation returns or unwinds.
/// Keeping the exclusive interpreter borrow inside this safe guard avoids raw pointers while
/// ensuring a native panic cannot leak script strictness into later work in the same realm.
struct HostScriptStrictScope<'a> {
    ctx: &'a mut Interp,
    previous: bool,
}

impl<'a> HostScriptStrictScope<'a> {
    fn enter(ctx: &'a mut Interp, strict: bool) -> Self {
        let previous = ctx.strict;
        ctx.strict = strict;
        Self { ctx, previous }
    }

    fn ctx(&mut self) -> &mut Interp {
        self.ctx
    }
}

impl Drop for HostScriptStrictScope<'_> {
    fn drop(&mut self) {
        self.ctx.strict = self.previous;
    }
}

/// Stack-like interpreter state to roll back if a native callback panics through a host Script.
/// JavaScript throws remain ordinary completions and are not rolled back.
struct HostScriptExecutionCheckpoint {
    depth: u32,
    native_top: usize,
    cur_site: u32,
    constructing: bool,
    super_call_ok: bool,
    new_target: Value,
    pending_new_target: Value,
    pending_tail: Option<Box<(Value, Value, Vec<Value>)>>,
    fn_frames: usize,
    jit_frames: usize,
    using_scopes: usize,
    native_super_overrides: usize,
    ctor_caller_realm: Option<RealmState>,
    pending_fn_name: Option<String>,
    short_circuit: bool,
    yield_raw_result: bool,
    in_async_gen_body: bool,
    in_field_init_code: bool,
    tco_ok: bool,
    decorator_initializers: usize,
}

impl HostScriptExecutionCheckpoint {
    fn capture(ctx: &Interp) -> Self {
        let pending_tail = ctx
            .pending_tail
            .as_ref()
            .map(|tail| Box::new((tail.0.clone(), tail.1.clone(), tail.2.clone())));
        Self {
            depth: ctx.depth,
            native_top: ctx.native_top,
            cur_site: ctx.cur_site,
            constructing: ctx.constructing,
            super_call_ok: ctx.super_call_ok,
            new_target: ctx.new_target.clone(),
            pending_new_target: ctx.pending_new_target.clone(),
            pending_tail,
            fn_frames: ctx.fn_frames.len(),
            jit_frames: ctx.jit_frames,
            using_scopes: ctx.using_stack.len(),
            native_super_overrides: ctx.native_super_return_overrides.len(),
            ctor_caller_realm: ctx
                .ctor_caller_realm
                .as_ref()
                .map(RealmState::snapshot_clone),
            pending_fn_name: ctx.pending_fn_name.clone(),
            short_circuit: ctx.short_circuit,
            yield_raw_result: ctx.yield_raw_result,
            in_async_gen_body: ctx.in_async_gen_body,
            in_field_init_code: ctx.in_field_init_code,
            tco_ok: ctx.tco_ok,
            decorator_initializers: ctx.decorator_initializers.len(),
        }
    }

    fn restore(self, ctx: &mut Interp) {
        ctx.depth = self.depth;
        ctx.native_top = self.native_top;
        ctx.cur_site = self.cur_site;
        ctx.constructing = self.constructing;
        ctx.super_call_ok = self.super_call_ok;
        ctx.new_target = self.new_target;
        ctx.pending_new_target = self.pending_new_target;
        ctx.pending_tail = self.pending_tail;
        ctx.fn_frames.truncate(self.fn_frames);
        ctx.jit_frames = self.jit_frames;
        ctx.using_stack.truncate(self.using_scopes);
        ctx.native_super_return_overrides
            .truncate(self.native_super_overrides);
        ctx.ctor_caller_realm = self.ctor_caller_realm;
        ctx.pending_fn_name = self.pending_fn_name;
        ctx.short_circuit = self.short_circuit;
        ctx.yield_raw_result = self.yield_raw_result;
        ctx.in_async_gen_body = self.in_async_gen_body;
        ctx.in_field_init_code = self.in_field_init_code;
        ctx.tco_ok = self.tco_ok;
        ctx.decorator_initializers
            .truncate(self.decorator_initializers);
    }
}

impl WindowProxyRegistry {
    pub(crate) fn sweep(&mut self, key: usize) {
        self.entries.remove(&key);
    }
}

pub(crate) fn authorized_window_target(interp: &Interp, proxy_key: usize) -> Option<usize> {
    let registry = interp.host_state.get::<WindowProxyRegistry>()?;
    let active_caller = Gc::as_ptr(&interp.global) as usize;
    let operation_target = registry.active_receivers.borrow().iter().rev().find_map(
        |(active_proxy, caller, target)| {
            (*active_proxy == proxy_key && *caller == active_caller).then_some(*target)
        },
    );
    if let Some(operation_target) = operation_target {
        if registry
            .entries
            .get(&proxy_key)
            .is_some_and(|entry| entry.target.key == operation_target)
        {
            return Some(operation_target);
        }
    }
    None
}

impl Interp {
    /// Authorize access to a native Window receiver after a previously obtained native method is
    /// invoked. Unlike the short Get/Set operation scope, this check can outlive the property
    /// access, so it asks the current host policy again and preserves its thrown value.
    pub(crate) fn window_proxy_native_target(
        &mut self,
        proxy_key: usize,
    ) -> Result<Option<usize>, Value> {
        if let Some(target) = authorized_window_target(self, proxy_key) {
            return Ok(Some(target));
        }
        let entry = self
            .host_state
            .get::<WindowProxyRegistry>()
            .and_then(|registry| registry.entries.get(&proxy_key))
            .cloned();
        let Some(entry) = entry else {
            return Ok(None);
        };
        if !entry.target.is_registered_in(self) {
            return Err(self.make_error("TypeError", "WindowProxy target realm is no longer valid"));
        }
        let active_key = Gc::as_ptr(&self.global) as usize;
        let caller = self.invocation_caller_handle();
        entry
            .policy
            .authorize_native_window_receiver(self, &caller, &entry.target)?;
        if Gc::as_ptr(&self.global) as usize != active_key {
            return Err(self.make_error(
                "TypeError",
                "WindowProxy receiver policy did not restore the active realm",
            ));
        }
        if !entry.target.is_registered_in(self) {
            return Err(self.make_error(
                "TypeError",
                "WindowProxy target realm changed during receiver authorization",
            ));
        }
        Ok(Some(entry.target.key))
    }
}

struct WindowProxyReceiverGuard {
    active: Rc<RefCell<Vec<(usize, usize, usize)>>>,
}

impl Drop for WindowProxyReceiverGuard {
    fn drop(&mut self) {
        self.active
            .borrow_mut()
            .pop()
            .expect("WindowProxy receiver scopes are balanced");
    }
}

/// Restores one interpreter's active realm while persisting the realm that was entered.
///
/// This private guard is constructed only inside `with_host_realm`; its raw pointer is valid
/// until the method returns, including unwinding through the callback.
struct RestoreRealmOnDrop {
    interp: *mut Interp,
    active_key: usize,
    caller: Option<RealmState>,
    scopes: Rc<std::cell::Cell<usize>>,
}

impl Drop for RestoreRealmOnDrop {
    fn drop(&mut self) {
        // SAFETY: `with_host_realm` retains the exclusive `&mut Interp` for the guard's entire
        // lifetime, and neither the guard nor its pointer is exposed to the callback. Nested
        // scopes have their own guard and unwind in stack order.
        let interp = unsafe { &mut *self.interp };
        let mut active = interp.snapshot_realm();
        if let Some(registered) = interp.realms.get_mut(&self.active_key) {
            active.collectable = registered.collectable;
            active.host_managed = registered.host_managed;
            *registered = active;
        }
        if let Some(caller) = self.caller.take() {
            interp.restore_realm(&caller);
        }
        self.scopes.set(self.scopes.get() - 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{abrupt_value, OpError, OpResult};
    use crate::value::WeakGc;
    use crate::Engine;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn specification_module_settings_own_graphs_pending_completions_and_retained_namespaces() {
        let mut engine = Engine::new();
        let child = engine.ctx().create_host_realm();
        let weak = engine.ctx().weak_value(&child.global()).expect("child global");
        let queue = Rc::new(std::cell::RefCell::new(Vec::new()));
        let request_queue = queue.clone();
        engine.ctx().with_host_realm(&child, |ctx| {
            ctx.install_module_fetch_loader(Rc::new(|request| Some(crate::ModuleFetchResult {
                key: request.specifier,
                source: if request.attribute_type.as_deref()==Some("text") { "captured text".into() }
                    else { "export const value = 17; export const global = globalThis;".into() },
                script_context: request.script_context,
            })));
            ctx.install_async_module_import_handler(Rc::new(move |request| request_queue.borrow_mut().push(request)));
            let css=ctx.dynamic_import("https://example.test/not-exposed.css",Some("css"),false,None);
            ctx.observe_promise_for_host(&css);
            assert!(matches!(crate::eval::promise_fast::promise_state(&css),Some((crate::eval::promise_fast::REJECTED,_))),"settings without CSSOM exposure reject before fetching");
            ctx.dynamic_import("https://example.test/module.js",None,false,None)
        }).expect("child import");
        let request = queue.borrow_mut().pop().expect("actual asynchronous request");
        let handle = engine.ctx().complete_prepared_module_import_for_host(request.id).expect("completion enters origin while parent active");
        assert!(matches!(engine.ctx().get_member(&handle.namespace,"value"),Ok(Value::Num(17.0))));
        let global = engine.ctx().get_member(&handle.namespace,"global").ok().expect("origin export");
        assert_eq!(global.object_identity(),child.global().object_identity());
        drop(global);
        let parent = engine.ctx().run_prepared_module_for_host("export const value = 29;","https://example.test/module.js","https://example.test/module.js","https://example.test/module.js",None).expect("parent graph");
        assert!(matches!(engine.ctx().get_member(&parent.namespace,"value"),Ok(Value::Num(29.0))));
        assert_ne!(parent.namespace.object_identity(),handle.namespace.object_identity());
        let context = Rc::new(crate::ClassicScriptContext { base_url:"https://cdn.test/async.js".into(),
            nonce:"captured".into(),credentials_mode:"include".into(),referrer_policy:"origin".into() });
        let asynchronous = engine.ctx().with_host_realm(&child,|ctx|
            ctx.run_prepared_module_for_host("await Promise.resolve(); export const global = globalThis; export const pending = import('./late.txt', {with:{type:'text'}});",
                "https://example.test/async.js","https://cdn.test/async.js","https://cdn.test/async.js",Some(context)))
            .expect("child async entry").expect("async module parses");
        engine.run_microtasks();
        let resumed_global = engine.ctx().get_member(&asynchronous.namespace,"global").ok().expect("resumed module export");
        assert_eq!(resumed_global.object_identity(),child.global().object_identity());
        drop(resumed_global);
        let resumed = queue.borrow_mut().pop().expect("import after top-level await");
        assert_eq!(resumed.settings_key,child.key());
        assert_eq!(resumed.attribute_type.as_deref(),Some("text"));
        assert_eq!(resumed.referrer,"https://cdn.test/async.js");
        assert_eq!(resumed.script_context.as_ref().expect("captured source options").nonce,"captured");
        let text = engine.ctx().complete_prepared_module_import_for_host(resumed.id).expect("resumed import completion");
        assert!(matches!(engine.ctx().get_member(&text.namespace,"default"),Ok(Value::Str(value)) if value.as_str()=="captured text"));
        drop(text);
        drop((asynchronous,resumed));
        engine.run_microtasks();
        engine.ctx().with_host_realm(&child, |ctx|ctx.dynamic_import("https://example.test/later.js",None,false,None)).expect("second import");
        let cancelled = queue.borrow_mut().pop().expect("pending request");
        engine.ctx().cancel_async_module_imports_for_realm(&child);
        assert!(!engine.ctx().has_pending_module_import_for_host(cancelled.id));
        assert!(engine.ctx().complete_prepared_module_import_for_host(cancelled.id).is_err());
        let legacy_calls = Rc::new(std::cell::Cell::new(0));
        let calls = legacy_calls.clone();
        engine.set_module_loader_attrs(move |_,_,_| { calls.set(calls.get()+1); Some(("legacy".into(),"export {};".into())) });
        let rejected = engine.ctx().with_host_realm(&child,|ctx| {
            let promise = ctx.dynamic_import("https://example.test/retired.js",None,false,None);
            ctx.observe_promise_for_host(&promise);
            promise
        }).expect("retained retired settings entry");
        assert!(matches!(crate::eval::promise_fast::promise_state(&rejected),Some((crate::eval::promise_fast::REJECTED,_))));
        assert_eq!(legacy_calls.get(),0,"retired browser settings cannot become Node loader requests");
        drop(rejected);
        engine.ctx().dispose_host_realm(&child).expect("retire child");
        drop((child,request,cancelled,queue));
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(),"retained namespace preserves its actual source settings");
        drop(handle);
        engine.run_microtasks();
        engine.collect_garbage();
        assert!(weak.upgrade().is_none(),"released namespace and cancelled requests cannot pin retired settings");
    }

    #[test]
    fn specification_window_root_realm_replacement_preserves_retained_realms_and_rejects_scopes() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let old = ctx.current_host_realm();
        let old_global = old.global();
        let weak_old = Gc::downgrade(old_global.as_obj().unwrap());
        let retained = ctx.eval_in_realm(&old_global, "globalThis.marker='old';()=>marker")
            .unwrap_or_else(|_| panic!("old root closure"));
        let next = ctx.create_host_realm();
        let next_global = next.global();
        ctx.eval_in_realm(&next_global, "globalThis.marker='new'").unwrap_or_else(|_| panic!("new root initialization"));
        assert!(matches!(ctx.with_host_realm(&next, |ctx| ctx.replace_root_host_realm(&next)),
            Ok(Err(HostRealmDisposeError::ActiveRealm))));
        assert!(ctx.current_host_realm().same_realm(&old));
        let retired = ctx.replace_root_host_realm(&next).expect("replace inactive root between turns");
        assert!(retired.same_realm(&old));
        assert!(ctx.current_host_realm().same_realm(&next));
        let marker = ctx.eval_in_realm(&next_global, "marker").unwrap_or_else(|_| panic!("new root lookup"));
        assert!(matches!(marker, Value::Str(value) if value.as_str()=="new"));
        ctx.dispose_host_realm(&retired).expect("retire previous default root");
        let result = ctx.call(retained.clone(), Value::Undefined, &[]).unwrap_or_else(|_| panic!("retained old root closure"));
        assert!(matches!(result, Value::Str(value) if value.as_str()=="old"));
        drop(old); drop(old_global); drop(retired);
        ctx.collect_garbage();
        assert!(weak_old.upgrade().is_some(), "retained function must keep its original realm");
        drop(retained);
        ctx.collect_garbage();
        assert!(weak_old.upgrade().is_none(), "an unreferenced former root must be collectible");
        assert!(ctx.current_host_realm().same_realm(&next));
    }

    #[lumen_bind::class(name = "HostRealmBase", hint(js(webidl)))]
    struct HostRealmBase {
        marker: i32,
    }

    #[lumen_bind::methods]
    impl HostRealmBase {
        #[constructor]
        fn new(marker: i32) -> Self {
            Self { marker }
        }

        #[getter]
        fn marker(&self) -> i32 {
            self.marker
        }
    }

    fn read_native_window_marker(
        ctx: &mut Interp,
        this: Value,
        _args: &[Value],
    ) -> Result<Value, Value> {
        let marker = ctx
            .with_instance::<HostRealmBase, _>(&this, |window| window.marker)
            .map_err(|error| error.to_value(ctx))?;
        Ok(Value::Num(marker as f64))
    }

    fn return_invocation_host_global(
        ctx: &mut Interp,
        _this: Value,
        _args: &[Value],
    ) -> Result<Value, Value> {
        Ok(ctx.invocation_host_realm().global())
    }

    fn panic_during_host_script(
        ctx: &mut Interp,
        _this: Value,
        _args: &[Value],
    ) -> Result<Value, Value> {
        // Simulate a native callback that temporarily changes interpreter execution state and
        // then unwinds. The host-script scope must restore its caller's strictness.
        ctx.strict = false;
        panic!("host script native panic sentinel");
    }

    #[lumen_bind::class(
        name = "HostRealmDerived",
        extends = HostRealmBase,
        hint(js(webidl))
    )]
    struct HostRealmDerived {
        base: HostRealmBase,
    }

    #[lumen_bind::methods]
    impl HostRealmDerived {
        #[constructor]
        fn new(marker: i32) -> Self {
            Self {
                base: HostRealmBase { marker },
            }
        }

        #[getter]
        fn doubled(&self) -> i32 {
            self.base.marker * 2
        }
    }

    #[derive(Default)]
    struct TestWindowProxyPolicy {
        deny_native_receiver: bool,
        deny_cross_realm_native_receiver: bool,
        wrong_get_result: bool,
        children: Rc<RefCell<Vec<WeakGc>>>,
    }

    impl WindowProxyPolicy for TestWindowProxyPolicy {
        fn decide(
            &self,
            _ctx: &mut Interp,
            _caller: &RealmHandle,
            _target: &RealmHandle,
            operation: &WindowProxyOperation,
        ) -> WindowProxyDisposition {
            match operation {
                WindowProxyOperation::Get { key, .. } if matches!(key, Value::Str(value) if value.as_ref() == "denied") => {
                    WindowProxyDisposition::Denied(Value::Num(73.0))
                }
                WindowProxyOperation::Get { .. } if self.wrong_get_result => {
                    WindowProxyDisposition::Handled(WindowProxyResult::Has(true))
                }
                _ => WindowProxyDisposition::ForwardSameOrigin,
            }
        }

        fn authorize_native_window_receiver(
            &self,
            ctx: &mut Interp,
            caller: &RealmHandle,
            target: &RealmHandle,
        ) -> Result<(), Value> {
            if self.deny_native_receiver
                || (self.deny_cross_realm_native_receiver && caller.key() != target.key())
            {
                Err(Value::Num(91.0))
            } else {
                let _ = ctx;
                Ok(())
            }
        }

        fn child_window_count(
            &self,
            _ctx: &mut Interp,
            _caller: &RealmHandle,
            _target: &RealmHandle,
        ) -> usize {
            self.children.borrow().len()
        }

        fn child_window_at(
            &self,
            _ctx: &mut Interp,
            _caller: &RealmHandle,
            _target: &RealmHandle,
            index: u32,
        ) -> Option<Value> {
            self.children
                .borrow()
                .get(index as usize)
                .and_then(WeakGc::upgrade)
                .map(Value::Obj)
        }
    }

    #[lumen_bind::class(name = "HostRealmIndexed", hint(js(webidl)))]
    struct HostRealmIndexed {
        values: Vec<i32>,
    }

    #[lumen_bind::methods]
    impl HostRealmIndexed {
        #[proto(getitem)]
        fn getitem(&self, index: usize) -> i32 {
            self.values.get(index).copied().unwrap_or(-1)
        }

        #[proto(len)]
        fn length(&self) -> usize {
            self.values.len()
        }
    }

    fn object_id(ctx: &Interp, value: &Value) -> usize {
        ctx.object_addr(value).expect("object identity")
    }

    fn member(ctx: &mut Interp, object: &Value, name: &str) -> Value {
        ctx.get_member(object, name)
            .unwrap_or_else(|_| panic!("missing member {name}"))
    }

    #[test]
    fn host_intrinsic_property_helpers_ignore_replaced_reflect_and_proxy() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let global = ctx.global_object();
        let target = ctx
            .eval_in_realm(&global, "({ get value(){ return this.tag; }, tag: 3 })")
            .unwrap_or_else(|_| panic!("create intrinsic reflection target"));
        let receiver = ctx
            .eval_in_realm(&global, "({ tag: 41 })")
            .unwrap_or_else(|_| panic!("create explicit Reflect receiver"));
        let sentinel = ctx
            .eval_in_realm(
                &global,
                "globalThis.reflectSentinel={}; \
                 globalThis.Reflect={ \
                     get(){ throw 'replaced get'; }, \
                     has(){ throw 'replaced has'; }, \
                     getOwnPropertyDescriptor(){ throw 'replaced gopd'; } \
                 }; \
                 globalThis.Proxy=function(){ throw 'replaced Proxy'; }; \
                 reflectSentinel",
            )
            .unwrap_or_else(|_| panic!("replace author-visible intrinsics"));

        let key = Value::str("value");
        assert!(matches!(ctx.reflect_has(&target, &key), Ok(true)));
        let value = ctx
            .reflect_get(&target, &key, &receiver)
            .unwrap_or_else(|_| panic!("intrinsic Reflect.get must retain its receiver"));
        assert!(matches!(value, Value::Num(number) if number == 41.0));
        let descriptor = ctx
            .reflect_get_own_property_descriptor(&target, &key)
            .unwrap_or_else(|_| panic!("intrinsic Reflect.getOwnPropertyDescriptor"));
        assert!(matches!(descriptor, Value::Obj(_)));
        let getter = ctx
            .member_get(&descriptor, "get")
            .unwrap_or_else(|_| panic!("accessor descriptor getter"));
        assert!(getter.is_callable());

        let proxy_target = ctx
            .eval_in_realm(&global, "({ answer: 42 })")
            .unwrap_or_else(|_| panic!("create target for intrinsic Proxy"));
        let handler = ctx
            .eval_in_realm(
                &global,
                "({ get(target,key){ return key==='answer' ? 'trapped' : undefined; }, \
                     has(target,key){ return key==='answer'; } })",
            )
            .unwrap_or_else(|_| panic!("create intrinsic Proxy handler"));
        let proxy = ctx
            .create_proxy(proxy_target, handler)
            .unwrap_or_else(|_| panic!("create Proxy without mutable global binding"));
        assert!(ctx.is_proxy_value(&proxy));
        assert!(matches!(
            ctx.reflect_get(&proxy, &Value::str("answer"), &proxy),
            Ok(Value::Str(value)) if value.as_ref() == "trapped"
        ));
        assert!(matches!(
            ctx.reflect_has(&proxy, &Value::str("answer")),
            Ok(true)
        ));

        let throwing_handler = ctx
            .eval_in_realm(&global, "({ has(){ throw globalThis.reflectSentinel; } })")
            .unwrap_or_else(|_| panic!("create throwing Proxy handler"));
        let empty_target = Value::Obj(ctx.new_object());
        let throwing_proxy = ctx
            .create_proxy(empty_target, throwing_handler)
            .unwrap_or_else(|_| panic!("create throwing Proxy"));
        let thrown = ctx
            .reflect_has(&throwing_proxy, &Value::str("anything"))
            .expect_err("Proxy has trap must propagate its thrown value");
        assert_eq!(object_id(ctx, &thrown), object_id(ctx, &sentinel));
    }

    #[test]
    fn host_global_this_uses_windowproxy_and_keeps_declarations_on_backing_global() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let first = ctx.create_host_realm();
        let first_global = first.global();
        let second = ctx.create_host_realm();
        let second_global = second.global();
        let proxy = ctx
            .create_window_proxy(&first, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create first browsing-context WindowProxy");
        let proxy_id = object_id(ctx, &proxy);

        ctx.set_host_global_this(&first, proxy.clone())
            .expect("publish the WindowProxy in its target realm");
        drop(proxy);
        ctx.collect_garbage();
        let proxy = member(ctx, &first_global, "globalThis");
        assert_eq!(object_id(ctx, &proxy), proxy_id);
        assert!(ctx
            .member_set(&first_global, "window", proxy.clone())
            .is_ok());
        let initialize = ctx
            .eval_in_realm(
                &first_global,
                "var declaredVar=13; let lexicalOnly=17; globalThis.expando=declaredVar; lexicalOnly===17;",
            )
            .unwrap_or_else(|_| panic!("initialize child Window globals"));
        assert!(matches!(initialize, Value::Bool(true)));
        for check in [
            "this===window",
            "window===globalThis",
            "globalThis.globalThis===globalThis",
            "Function('return this')()===globalThis",
            "Function('\"use strict\"; return this')()===undefined",
            "declaredVar===13",
            "globalThis.expando===13",
            "Object.getOwnPropertyDescriptor(globalThis,'expando').value===13",
            "globalThis.lexicalOnly===undefined",
            "Object.getOwnPropertyDescriptor(globalThis,'globalThis').value===globalThis",
            "Object.getOwnPropertyDescriptor(globalThis,'globalThis').writable",
            "!Object.getOwnPropertyDescriptor(globalThis,'globalThis').enumerable",
            "Object.getOwnPropertyDescriptor(globalThis,'globalThis').configurable",
        ] {
            let result = ctx
                .eval_in_realm(&first_global, check)
                .unwrap_or_else(|_| panic!("evaluate child Window invariant: {check}"));
            assert!(
                matches!(result, Value::Bool(true)),
                "child Window invariant failed: {check}"
            );
        }
        let receiver_contract = ctx
            .eval_in_realm(
                &first_global,
                "(()=>{ const isolated={}; \
                    const isolatedSet=Reflect.set(window,'receiverOnly',23,isolated) && \
                        isolated.receiverOnly===23 && \
                        !Object.prototype.hasOwnProperty.call(window,'receiverOnly'); \
                    const windowSet=Reflect.set(window,'reflectAssigned',29,window) && \
                        window.reflectAssigned===29 && \
                        Object.prototype.hasOwnProperty.call(window,'reflectAssigned'); \
                    return isolatedSet && windowSet })()",
            )
            .unwrap_or_else(|_| panic!("evaluate exact WindowProxy [[Set]] receiver contract"));
        assert!(matches!(receiver_contract, Value::Bool(true)));
        assert!(matches!(
            ctx.member_get(&first_global, "declaredVar"),
            Ok(Value::Num(value)) if value == 13.0
        ));
        assert_eq!(
            object_id(ctx, &ctx.global_this()),
            object_id(ctx, &parent_global)
        );

        ctx.retarget_window_proxy(&proxy, &second)
            .expect("navigate the browsing context");
        ctx.set_host_global_this(&second, proxy.clone())
            .expect("publish the same WindowProxy in the new document realm");
        assert!(ctx
            .member_set(&second_global, "window", proxy.clone())
            .is_ok());
        let second_checks = ctx
            .eval_in_realm(
                &second_global,
                "var secondVar=29; this===window && window===globalThis && \
                 Function('return this')()===globalThis && globalThis.secondVar===29",
            )
            .unwrap_or_else(|_| panic!("evaluate navigated child Window globals"));
        assert!(matches!(second_checks, Value::Bool(true)));

        let old_document_checks = ctx
            .eval_in_realm(
                &first_global,
                "this===window && window===globalThis && globalThis.secondVar===29 && declaredVar===13",
            )
            .unwrap_or_else(|_| panic!("evaluate a retained old-document script realm"));
        assert!(matches!(old_document_checks, Value::Bool(true)));
        assert!(matches!(
            ctx.member_get(&first_global, "declaredVar"),
            Ok(Value::Num(value)) if value == 13.0
        ));
        assert!(matches!(
            ctx.member_get(&second_global, "secondVar"),
            Ok(Value::Num(value)) if value == 29.0
        ));
    }

    #[test]
    fn author_global_this_mutation_does_not_change_internal_global_this() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let target = ctx.create_host_realm();
        let target_global = target.global();
        let proxy = ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create child WindowProxy");
        ctx.set_host_global_this(&target, proxy.clone())
            .expect("publish child WindowProxy");
        assert!(ctx
            .member_set(&target_global, "window", proxy.clone())
            .is_ok());

        let result = ctx
            .eval_in_realm(
                &target_global,
                "const publishedThis=this; \
                 globalThis={replaced:true}; \
                 const overwriteWorks=globalThis.replaced && this===window && \
                     window===publishedThis && Function('return this')()===publishedThis; \
                 const deleteWorks=delete window.globalThis && \
                     typeof globalThis==='undefined' && this===window && \
                     window===publishedThis && Function('return this')()===publishedThis; \
                 overwriteWorks && deleteWorks",
            )
            .unwrap_or_else(|_| panic!("author globalThis mutation must preserve internal this"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn promise_intrinsic_cache_collects_retired_realms_and_preserves_inflight_bundles() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let realm = ctx.create_host_realm();
        let global = realm.global();
        let weak_global = ctx.weak_value(&global).expect("realm global");
        let bundle = ctx.with_host_realm(&realm, |ctx| {
            ctx.eval_in_realm(&global, "Promise.resolve(1); undefined")
                .unwrap_or_else(|_| panic!("initialize promise fast path"));
            ctx.promise_intr().expect("installed promise intrinsics")
        }).expect("enter child realm");
        let weak_then = ctx.weak_value(&Value::Obj(bundle.then.clone())).expect("original then");
        drop(bundle);
        ctx.eval_in_realm(&global, "Promise.prototype.then = null; Promise.resolve = null")
            .unwrap_or_else(|_| panic!("replace public promise properties"));
        ctx.collect_garbage();
        assert!(weak_then.upgrade().is_some(), "live cached realm retains its original methods");
        let bundle = ctx.with_host_realm(&realm, |ctx| ctx.promise_intr().expect("live cached intrinsics"))
            .expect("enter live child realm");
        ctx.dispose_host_realm(&realm).expect("retire child realm");
        drop(global);
        drop(realm);
        ctx.collect_garbage();
        assert!(weak_global.upgrade().is_some(), "an executing intrinsic bundle owns its realm");
        drop(bundle);
        ctx.collect_garbage();
        assert!(weak_global.upgrade().is_none(), "cache bookkeeping does not pin a retired realm");
        assert!(weak_then.upgrade().is_none(), "unreachable original methods are released");
    }

    #[test]
    fn retired_host_realm_survives_through_proxy_and_closure_then_collects() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let retired = ctx.create_host_realm();
        let retired_key = retired.key();
        let retired_global = retired.global();
        let proxy = ctx
            .create_window_proxy(&retired, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create child WindowProxy");
        ctx.set_host_global_this(&retired, proxy.clone())
            .expect("publish child WindowProxy");
        assert!(ctx
            .member_set(&retired_global, "window", proxy.clone())
            .is_ok());
        let retained_function = ctx
            .eval_in_realm(
                &retired_global,
                "(function(){ return { \
                    intrinsicPrototypePreserved:Object.getPrototypeOf({})===Object.prototype, \
                    oldArray:Array, windowArray:this.Array, thisIsPublished:this===globalThis, \
                    sloppyThisTargetsNewRealm:Function('return this')().Array!==Array \
                } })",
            )
            .unwrap_or_else(|_| panic!("create a child-realm closure"));

        let active_error = ctx.dispose_host_realm(&parent);
        assert_eq!(active_error, Err(HostRealmDisposeError::ActiveRealm));
        ctx.dispose_host_realm(&retired)
            .expect("retirement marks the realm collectable without discarding live proxy state");
        drop(retired_global);
        drop(retired);
        ctx.collect_garbage();
        assert!(ctx.realms.contains_key(&retired_key));
        let still_live = ctx
            .call(retained_function.clone(), Value::Undefined, &[])
            .unwrap_or_else(|_| panic!("retained child closure must keep its realm live"));
        assert!(matches!(
            member(ctx, &still_live, "intrinsicPrototypePreserved"),
            Value::Bool(true)
        ));
        assert!(matches!(
            member(ctx, &still_live, "thisIsPublished"),
            Value::Bool(true)
        ));
        let old_array = member(ctx, &still_live, "oldArray");
        let window_array = member(ctx, &still_live, "windowArray");
        assert_eq!(object_id(ctx, &old_array), object_id(ctx, &window_array));
        drop(old_array);
        drop(window_array);
        assert!(matches!(
            member(ctx, &still_live, "sloppyThisTargetsNewRealm"),
            Value::Bool(false)
        ));
        drop(still_live);

        let next = ctx.create_host_realm();
        let next_key = next.key();
        let next_global = next.global();
        ctx.retarget_window_proxy(&proxy, &next)
            .expect("navigate stable WindowProxy");
        ctx.set_host_global_this(&next, proxy.clone())
            .expect("publish WindowProxy in next document");
        assert!(ctx
            .member_set(&next_global, "window", proxy.clone())
            .is_ok());
        ctx.collect_garbage();
        assert!(ctx.realms.contains_key(&retired_key));
        let after_navigation = ctx
            .call(retained_function.clone(), Value::Undefined, &[])
            .unwrap_or_else(|_| panic!("retained function remains callable after navigation"));
        assert!(matches!(
            member(ctx, &after_navigation, "intrinsicPrototypePreserved"),
            Value::Bool(true)
        ));
        assert!(matches!(
            member(ctx, &after_navigation, "thisIsPublished"),
            Value::Bool(true)
        ));
        assert!(matches!(
            member(ctx, &after_navigation, "sloppyThisTargetsNewRealm"),
            Value::Bool(true)
        ));
        let old_array = member(ctx, &after_navigation, "oldArray");
        let window_array = member(ctx, &after_navigation, "windowArray");
        assert_ne!(object_id(ctx, &old_array), object_id(ctx, &window_array));
        drop(old_array);
        drop(window_array);
        drop(after_navigation);
        drop(retained_function);
        drop(proxy);
        drop(next_global);
        drop(next);
        ctx.collect_garbage();
        assert!(!ctx.realms.contains_key(&retired_key));
        assert!(!ctx.realms.contains_key(&next_key));
    }

    #[test]
    fn repeated_host_navigation_collects_retired_realm_snapshots() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let mut current = ctx.create_host_realm();
        let proxy = ctx
            .create_window_proxy(&current, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create stable browsing-context WindowProxy");
        ctx.set_host_global_this(&current, proxy.clone())
            .expect("publish initial WindowProxy");

        for _ in 0..24 {
            let next = ctx.create_host_realm();
            let next_global = next.global();
            ctx.retarget_window_proxy(&proxy, &next)
                .expect("retarget the browsing context");
            ctx.set_host_global_this(&next, proxy.clone())
                .expect("publish WindowProxy in the new realm");
            assert!(ctx
                .member_set(&next_global, "window", proxy.clone())
                .is_ok());

            let retired_key = current.key();
            ctx.dispose_host_realm(&current)
                .expect("retire the previous host realm");
            drop(current);
            drop(next_global);
            ctx.collect_garbage();
            assert!(!ctx.realms.contains_key(&retired_key));
            assert!(ctx.realms.len() <= 2, "retired realm snapshots accumulated");
            current = next;
        }

        let final_key = current.key();
        ctx.retarget_window_proxy(&proxy, &parent)
            .expect("detach the browsing context from its final child realm");
        ctx.dispose_host_realm(&current)
            .expect("retire the final host realm");
        drop(current);
        drop(proxy);
        ctx.collect_garbage();
        assert!(!ctx.realms.contains_key(&final_key));
        assert_eq!(ctx.realms.len(), 1, "only the main realm should remain");
    }

    #[test]
    fn host_global_this_rejects_unregistered_values_and_wrong_targets() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let target = ctx.create_host_realm();
        let unrelated = ctx.create_host_realm();
        let proxy = ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create registered WindowProxy");

        assert_eq!(
            ctx.set_host_global_this(&target, Value::Num(1.0)),
            Err(HostGlobalThisError::NotObject)
        );
        assert_eq!(
            ctx.set_host_global_this(&unrelated, proxy.clone()),
            Err(HostGlobalThisError::WindowProxyTargetsDifferentRealm)
        );
        let author_proxy = ctx
            .eval_in_realm(&parent_global, "new Proxy({}, {})")
            .unwrap_or_else(|_| panic!("create author Proxy"));
        assert_eq!(
            ctx.set_host_global_this(&target, author_proxy),
            Err(HostGlobalThisError::NotGlobalOrWindowProxy)
        );

        let mut other_engine = Engine::new();
        let foreign_realm = other_engine.ctx().current_host_realm();
        assert_eq!(
            ctx.set_host_global_this(&foreign_realm, foreign_realm.global()),
            Err(HostGlobalThisError::DifferentInterpreter)
        );
        assert_eq!(
            object_id(ctx, &ctx.global_this()),
            object_id(ctx, &parent_global)
        );
    }

    #[test]
    fn window_proxy_forwards_reflection_with_exact_receiver_and_retargets_stably() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let first = ctx.create_host_realm();
        let second = ctx.create_host_realm();
        let grandchild = ctx.create_host_realm();
        let first_global = first.global();
        let second_global = second.global();

        for (realm, global, answer) in [(&first, &first_global, 41), (&second, &second_global, 42)]
        {
            ctx.with_host_realm(realm, |ctx| {
                let source = format!(
                    "globalThis.answer={answer}; \
                     Object.defineProperty(globalThis,'reflectReceiver',{{get(){{return this}},configurable:true}}); \
                     Object.defineProperty(globalThis,'reflectSetter',{{set(v){{this.saved=v}},configurable:true}});"
                );
                assert!(ctx.eval_in_realm(global, &source).is_ok());
            })
            .expect("enter Window realm");
        }

        let policy = Rc::new(TestWindowProxyPolicy::default());
        let child_proxy = ctx
            .create_window_proxy(&grandchild, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create child WindowProxy");
        policy
            .children
            .borrow_mut()
            .push(Gc::downgrade(child_proxy.as_obj().expect("proxy object")));
        let proxy = ctx
            .create_window_proxy(&first, policy)
            .expect("create WindowProxy");
        assert!(ctx
            .member_set(&parent_global, "windowProxy", proxy.clone())
            .is_ok());
        assert!(ctx
            .member_set(&parent_global, "savedWindowProxy", proxy.clone())
            .is_ok());
        assert!(ctx
            .member_set(&parent_global, "childWindowProxy", child_proxy)
            .is_ok());

        let checks = ctx
            .eval_in_realm(
                &parent_global,
                "windowProxy.answer===41 && windowProxy===savedWindowProxy && \
                 Reflect.get(windowProxy,'reflectReceiver',savedWindowProxy)===savedWindowProxy && \
                 Reflect.get(windowProxy,'reflectReceiver',globalThis)===globalThis && \
                 Reflect.set(windowProxy,'reflectSetter',19,globalThis) && globalThis.saved===19 && \
                 Reflect.get(windowProxy,'0')===childWindowProxy && '0' in windowProxy && \
                 Reflect.getOwnPropertyDescriptor(windowProxy,'0').writable===false && \
                 Reflect.getOwnPropertyDescriptor(windowProxy,'0').enumerable===true && \
                 Reflect.getOwnPropertyDescriptor(windowProxy,'0').configurable===true && \
                 Reflect.ownKeys(windowProxy)[0]==='0' && \
                 Object.prototype.propertyIsEnumerable.call(windowProxy,'0') && \
                 Object.keys(windowProxy).includes('answer') && \
                 Reflect.set(windowProxy,'0',7)===false && Reflect.deleteProperty(windowProxy,'0')===false && \
                 Object.isExtensible(windowProxy) && Reflect.isExtensible(windowProxy) && \
                 Reflect.preventExtensions(windowProxy)===false && \
                 Reflect.getPrototypeOf(windowProxy)===Object.getPrototypeOf(windowProxy) && \
                 Reflect.setPrototypeOf(windowProxy,Object.getPrototypeOf(windowProxy)) && \
                 !Reflect.setPrototypeOf(windowProxy,null)",
            )
            .unwrap_or_else(|_| panic!("WindowProxy operations should not throw"));
        assert!(matches!(checks, Value::Bool(true)));

        ctx.retarget_window_proxy(&proxy, &second)
            .expect("retarget WindowProxy");
        let after = ctx
            .eval_in_realm(
                &parent_global,
                "windowProxy===savedWindowProxy && windowProxy.answer===42",
            )
            .unwrap_or_else(|_| panic!("read retargeted WindowProxy"));
        assert!(matches!(after, Value::Bool(true)));
    }

    #[test]
    fn window_proxy_denials_keep_the_host_thrown_value_and_policy_results_are_typed() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let target = ctx.create_host_realm();
        let denied = ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create denied WindowProxy");
        let wrong = ctx
            .create_window_proxy(
                &target,
                Rc::new(TestWindowProxyPolicy {
                    wrong_get_result: true,
                    ..TestWindowProxyPolicy::default()
                }),
            )
            .expect("create mistyped WindowProxy policy");
        assert!(ctx
            .member_set(&parent_global, "deniedWindow", denied)
            .is_ok());
        assert!(ctx.member_set(&parent_global, "wrongWindow", wrong).is_ok());
        let result = ctx
            .eval_in_realm(
                &parent_global,
                "(()=>{let denied=false, typed=false; \
                 try{deniedWindow.denied}catch(error){denied=error===73} \
                 try{wrongWindow.answer}catch(error){typed=error instanceof TypeError} \
                 return denied&&typed})()",
            )
            .unwrap_or_else(|_| panic!("denial checks should be catchable"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_window_receiver_authorization_preserves_errors_and_rejects_author_proxies() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let target = ctx.create_host_realm();
        let target_global = target.global();
        let saved_proto = target_global
            .as_obj()
            .expect("global object")
            .borrow()
            .proto
            .clone();
        ctx.with_host_realm(&target, |ctx| {
            ctx.attach_instance(&target_global, HostRealmBase { marker: 55 })
                .expect("attach native Window data");
            target_global
                .as_obj()
                .expect("global object")
                .borrow_mut()
                .proto = saved_proto.clone();
        })
        .expect("enter target Window realm");

        let allowed = ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create authorized WindowProxy");
        assert!(ctx
            .with_instance::<HostRealmBase, _>(&allowed, |window| window.marker)
            .is_ok_and(|marker| marker == 55));

        let denied = ctx
            .create_window_proxy(
                &target,
                Rc::new(TestWindowProxyPolicy {
                    deny_native_receiver: true,
                    ..TestWindowProxyPolicy::default()
                }),
            )
            .expect("create receiver-denied WindowProxy");
        let thrown = ctx
            .with_instance::<HostRealmBase, _>(&denied, |window| window.marker)
            .expect_err("native receiver policy must run before projection")
            .to_value(ctx);
        assert!(matches!(thrown, Value::Num(value) if value == 91.0));

        assert!(ctx
            .member_set(&parent_global, "windowForProxyTest", allowed)
            .is_ok());
        let author_proxy = ctx
            .eval_in_realm(&parent_global, "new Proxy(windowForProxyTest,{})")
            .unwrap_or_else(|_| panic!("create author Proxy"));
        assert!(ctx
            .with_instance::<HostRealmBase, _>(&author_proxy, |_| ())
            .is_err());
    }

    #[test]
    fn native_window_method_authorizes_the_original_invocation_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.current_host_realm();
        let parent_global = parent.global();
        let target = ctx.create_host_realm();
        let target_global = target.global();
        let target_object = target_global.as_obj().expect("target Window").clone();
        let saved_proto = target_object.borrow().proto.clone();
        ctx.with_host_realm(&target, |ctx| {
            ctx.attach_instance(&target_global, HostRealmBase { marker: 55 })
                .expect("attach target Window data");
            ctx.def_method(
                &target_object,
                "readNativeMarker",
                0,
                read_native_window_marker,
            );
            target_object.borrow_mut().proto = saved_proto.clone();
        })
        .expect("install target-realm native method");

        let proxy = ctx
            .create_window_proxy(
                &target,
                Rc::new(TestWindowProxyPolicy {
                    deny_cross_realm_native_receiver: true,
                    ..TestWindowProxyPolicy::default()
                }),
            )
            .expect("create policy-checked WindowProxy");
        assert!(ctx
            .member_set(&parent_global, "checkedWindow", proxy)
            .is_ok());
        let result = ctx
            .eval_in_realm(
                &parent_global,
                "(()=>{try{checkedWindow.readNativeMarker();return false}catch(error){return error===91}})()",
            )
            .unwrap_or_else(|_| panic!("cross-realm native receiver denial should be catchable"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn callback_host_realm_uses_get_function_realm_and_keeps_retired_functions_usable() {
        let mut engine = Engine::new();
        let child = engine.ctx().create_host_realm();
        let callback = engine.eval_value_in_host_realm(&child,
            "new Proxy((() => 42).bind(null), {})", false).unwrap().ok().expect("callback test script threw");
        let callback = crate::embed::JsFunction::from_value(callback).unwrap();
        assert!(engine.ctx().function_host_realm(&callback).ok().expect("callback realm unavailable").same_realm(&child));
        engine.ctx().dispose_host_realm(&child).unwrap();
        engine.ctx().collect_garbage();
        let owner = engine.ctx().function_host_realm(&callback).ok().expect("callback realm unavailable");
        assert!(owner.same_realm(&child));
        assert!(matches!(callback.call(engine.ctx(), Value::Undefined, &[]).unwrap(), Value::Num(42.0)));
        let revoked = engine.eval_value_in_host_realm(&child,
            "(() => {const pair=Proxy.revocable(()=>0,{});pair.revoke();return pair.proxy})()",
            false).unwrap().ok().expect("callback test script threw");
        let revoked = crate::embed::JsFunction::from_value(revoked).unwrap();
        assert!(engine.ctx().function_host_realm(&revoked).is_err());
    }

    #[test]
    fn public_invocation_host_realm_survives_native_function_realm_entry() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let caller = ctx.current_host_realm();
        let caller_global = caller.global();
        let target = ctx.create_host_realm();
        let target_global = target.global();
        let target_object = target_global.as_obj().expect("target Window").clone();
        let saved_proto = target_object.borrow().proto.clone();
        ctx.with_host_realm(&target, |ctx| {
            ctx.def_method(
                &target_object,
                "invocationGlobal",
                0,
                return_invocation_host_global,
            );
            target_object.borrow_mut().proto = saved_proto.clone();
        })
        .expect("install target-realm native method");

        let proxy = ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create cross-realm WindowProxy");
        assert!(ctx.member_set(&caller_global, "childWindow", proxy).is_ok());
        let observed = ctx
            .eval_in_realm(&caller_global, "childWindow.invocationGlobal()")
            .unwrap_or_else(|_| panic!("invoke target-realm native method"));
        assert_eq!(object_id(ctx, &observed), object_id(ctx, &caller_global));
    }

    #[test]
    fn window_proxy_rejects_a_target_from_another_interpreter_and_collects_dead_holders() {
        let mut first = Engine::new();
        let first_ctx = first.ctx();
        let target = first_ctx.create_host_realm();
        let proxy = first_ctx
            .create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create WindowProxy");
        let weak_proxy = first_ctx.weak_value(&proxy).expect("weak proxy");
        let mut second = Engine::new();
        let second_ctx = second.ctx();
        assert!(matches!(
            second_ctx.create_window_proxy(&target, Rc::new(TestWindowProxyPolicy::default())),
            Err(WindowProxyError::DifferentInterpreter)
        ));
        drop(proxy);
        let _ = first_ctx.collect_garbage();
        assert!(weak_proxy.upgrade().is_none());
    }

    #[test]
    fn evaluating_the_active_initial_realm_keeps_live_host_bindings() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let global = ctx.global_object();
        assert!(ctx
            .member_set(&global, "hostCounter", Value::Num(10.0))
            .is_ok());
        assert!(matches!(
            ctx.eval_in_realm(&global, "++hostCounter"),
            Ok(Value::Num(11.0))
        ));
        assert!(matches!(
            ctx.eval_in_realm(&global, "++hostCounter"),
            Ok(Value::Num(12.0))
        ));
        let unrelated = Value::Obj(ctx.new_object());
        assert!(ctx.eval_in_realm(&unrelated, "hostCounter").is_err());
        assert_eq!(
            object_id(ctx, &ctx.global_object()),
            object_id(ctx, &global)
        );
    }

    #[test]
    fn host_script_lexical_declarations_persist_across_scripts() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let child = ctx.create_host_realm();
        let proxy = ctx
            .create_window_proxy(&child, Rc::new(TestWindowProxyPolicy::default()))
            .expect("create child WindowProxy");
        ctx.set_host_global_this(&child, proxy)
            .expect("publish child WindowProxy");

        let first =
            crate::parser::parse_script("let persistentLexical=17; var persistentVar=19;", false)
                .expect("parse first host script");
        let first_result = ctx
            .with_host_realm(&child, |ctx| ctx.run_program_parsed(&first))
            .expect("enter child realm for first host script");
        assert!(first_result.is_ok(), "first host script should execute");

        let second = crate::parser::parse_script(
            "persistentLexical===17 && persistentVar===19 && globalThis.persistentLexical===undefined && globalThis.persistentVar===19;",
            false,
        )
        .expect("parse second host script");
        let second_result = ctx
            .with_host_realm(&child, |ctx| ctx.run_program_parsed(&second))
            .expect("enter child realm for second host script");
        assert!(matches!(second_result, Ok(Value::Bool(true))));
    }

    #[test]
    fn host_script_panic_restores_strictness_and_active_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_global = ctx.global_object();
        let parent_id = object_id(ctx, &parent_global);
        ctx.strict = true;

        let child = ctx.create_host_realm();
        ctx.with_host_realm(&child, |ctx| {
            let Value::Obj(global) = ctx.global_object() else {
                panic!("child realm global is an object");
            };
            ctx.def_method(&global, "panicHostScript", 0, panic_during_host_script);
        })
        .expect("enter child realm to install test native");
        assert!(ctx.strict, "direct host setup preserves caller strictness");
        let depth_before = ctx.depth;
        let native_top_before = ctx.native_top;
        let site_before = ctx.cur_site;
        let fn_frames_before = ctx.fn_frames.len();

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = ctx.eval_value_in_host_realm(&child, "'use strict'; panicHostScript()", false);
        }));
        assert!(
            panic.is_err(),
            "native callback should unwind through script eval"
        );
        assert!(ctx.strict, "parent strictness should survive child unwind");
        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);
        assert_eq!(ctx.depth, depth_before, "call depth should be restored");
        assert_eq!(
            ctx.native_top, native_top_before,
            "native frame should be restored"
        );
        assert_eq!(ctx.cur_site, site_before, "call site should be restored");
        assert_eq!(
            ctx.fn_frames.len(),
            fn_frames_before,
            "script frames should be restored"
        );
        let _trace_probe = ctx.make_error("Error", "stack after host panic");

        let child_eval = match ctx.eval_value_in_host_realm(
            &child,
            "var afterHostPanic = 23; this === globalThis && afterHostPanic === 23",
            false,
        ) {
            Ok(Ok(value)) => value,
            Ok(Err(_)) => panic!("child script executes after panic"),
            Err(error) => panic!("child script parses after panic: {error}"),
        };
        assert!(matches!(child_eval, Value::Bool(true)));
        assert!(ctx.strict, "child execution must restore parent strictness");
        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);
        let parent_isolated = ctx
            .eval_in_realm(&parent_global, "typeof afterHostPanic === 'undefined'")
            .unwrap_or_else(|_| panic!("parent global check executes"));
        assert!(matches!(parent_isolated, Value::Bool(true)));
    }

    #[test]
    fn host_realms_have_distinct_intrinsics_and_nested_scopes_persist_updates() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_global = ctx.global_object();
        let parent_id = object_id(ctx, &parent_global);
        let parent_object = member(ctx, &parent_global, "Object");
        let parent_object_proto = member(ctx, &parent_object, "prototype");

        let child = ctx.create_host_realm();
        let child_global = child.global();
        let child_id = object_id(ctx, &child_global);
        assert_ne!(parent_id, child_id);

        let grandchild = ctx
            .with_host_realm(&child, |ctx| {
                assert_eq!(object_id(ctx, &ctx.global_object()), child_id);
                let child_object = member(ctx, &child_global, "Object");
                let child_object_proto = member(ctx, &child_object, "prototype");
                assert_ne!(
                    object_id(ctx, &parent_object_proto),
                    object_id(ctx, &child_object_proto)
                );
                assert!(ctx
                    .member_set(&child_global, "hostMutation", Value::Num(42.0))
                    .is_ok());

                let grandchild = ctx.create_host_realm();
                let grandchild_id = object_id(ctx, &grandchild.global());
                let nested = ctx.with_host_realm(&grandchild, |ctx| {
                    assert_eq!(object_id(ctx, &ctx.global_object()), grandchild_id);
                    assert!(ctx
                        .member_set(&grandchild.global(), "nestedMutation", Value::Bool(true))
                        .is_ok());
                });
                assert!(nested.is_ok());
                assert_eq!(object_id(ctx, &ctx.global_object()), child_id);
                grandchild
            })
            .expect("enter child realm");

        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);
        let child_value = ctx
            .with_host_realm(&child, |ctx| member(ctx, &child_global, "hostMutation"))
            .expect("re-enter child realm");
        assert!(matches!(child_value, Value::Num(value) if value == 42.0));
        let nested_value = ctx
            .with_host_realm(&grandchild, |ctx| {
                member(ctx, &grandchild.global(), "nestedMutation")
            })
            .expect("re-enter grandchild realm");
        assert!(matches!(nested_value, Value::Bool(true)));
        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);
    }

    #[test]
    fn native_classes_get_realm_local_constructor_prototype_and_instance_chains() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_global = ctx.global_object();
        let parent_base = ctx.class_constructor::<HostRealmBase>();
        let parent_derived = ctx.class_constructor::<HostRealmDerived>();
        let parent_base_proto = member(ctx, &parent_base, "prototype");
        let parent_derived_proto = member(ctx, &parent_derived, "prototype");
        assert_eq!(
            object_id(ctx, &ctx.prototype_of(&parent_derived_proto)),
            object_id(ctx, &parent_base_proto)
        );
        assert!(ctx
            .member_set(&parent_global, "HostRealmBase", parent_base.clone())
            .is_ok());
        assert!(ctx
            .member_set(&parent_global, "HostRealmDerived", parent_derived.clone())
            .is_ok());
        let parent_indexed_constructor = ctx.class_constructor::<HostRealmIndexed>();
        assert!(ctx
            .member_set(
                &parent_global,
                "HostRealmIndexed",
                parent_indexed_constructor
            )
            .is_ok());
        let parent_instance = ctx.new_instance(HostRealmDerived {
            base: HostRealmBase { marker: 21 },
        });
        assert!(ctx
            .member_set(&parent_global, "parentRealmProbe", parent_instance.clone())
            .is_ok());
        let parent_indexed = ctx.new_instance(HostRealmIndexed { values: vec![11] });
        assert!(ctx
            .member_set(&parent_global, "parentIndexed", parent_indexed.clone())
            .is_ok());

        let child = ctx.create_host_realm();
        let child_global = child.global();
        let (child_base, child_derived, child_instance, child_checks) = ctx
            .with_host_realm(&child, |ctx| {
                let child_base = ctx.class_constructor::<HostRealmBase>();
                let child_derived = ctx.class_constructor::<HostRealmDerived>();
                let child_indexed_constructor = ctx.class_constructor::<HostRealmIndexed>();
                let child_base_proto = member(ctx, &child_base, "prototype");
                let child_derived_proto = member(ctx, &child_derived, "prototype");
                assert_ne!(object_id(ctx, &parent_base), object_id(ctx, &child_base));
                assert_ne!(object_id(ctx, &parent_derived), object_id(ctx, &child_derived));
                assert_ne!(
                    object_id(ctx, &parent_base_proto),
                    object_id(ctx, &child_base_proto)
                );
                assert_eq!(
                    object_id(ctx, &ctx.prototype_of(&child_derived_proto)),
                    object_id(ctx, &child_base_proto)
                );

                assert!(ctx
                    .member_set(&child_global, "HostRealmBase", child_base.clone())
                    .is_ok());
                assert!(ctx
                    .member_set(&child_global, "HostRealmDerived", child_derived.clone())
                    .is_ok());
                assert!(ctx
                    .member_set(
                        &child_global,
                        "HostRealmIndexed",
                        child_indexed_constructor
                    )
                    .is_ok());
                assert!(ctx
                    .member_set(
                        &child_global,
                        "parentRealmProbe",
                        parent_instance.clone()
                    )
                    .is_ok());
                assert!(ctx
                    .member_set(&child_global, "parentIndexed", parent_indexed.clone())
                    .is_ok());
                let child_indexed = ctx.new_instance(HostRealmIndexed { values: vec![22] });
                assert!(ctx
                    .member_set(&child_global, "childIndexed", child_indexed)
                    .is_ok());
                let child_instance = ctx.new_instance(HostRealmDerived {
                    base: HostRealmBase { marker: 23 },
                });
                assert!(ctx
                    .member_set(&child_global, "childRealmProbe", child_instance.clone())
                    .is_ok());
                let checks = ctx
                    .eval_in_realm(
                        &child_global,
                        "Object.getPrototypeOf(childRealmProbe)===HostRealmDerived.prototype && Object.getPrototypeOf(HostRealmDerived.prototype)===HostRealmBase.prototype && childRealmProbe instanceof HostRealmDerived && childRealmProbe.marker===23 && childRealmProbe.doubled===46 && !(parentRealmProbe instanceof HostRealmDerived) && childIndexed.length===1 && childIndexed[0]===22 && parentIndexed[0]===11 && !(parentIndexed instanceof HostRealmIndexed) && (new HostRealmDerived(25)).doubled===50",
                    )
                    .unwrap_or_else(|_| panic!("child native class checks threw"));
                (child_base, child_derived, child_instance, checks)
            })
            .expect("enter child realm");

        assert!(matches!(child_checks, Value::Bool(true)));
        assert_ne!(object_id(ctx, &parent_base), object_id(ctx, &child_base));
        assert_ne!(object_id(ctx, &parent_derived), object_id(ctx, &child_derived));
        assert!(ctx
            .member_set(&parent_global, "HostRealmBase", parent_base.clone())
            .is_ok());
        assert!(ctx
            .member_set(&parent_global, "HostRealmDerived", parent_derived.clone())
            .is_ok());
        assert!(ctx
            .member_set(&parent_global, "childRealmProbe", child_instance.clone())
            .is_ok());
        let child_indexed = ctx
            .with_host_realm(&child, |ctx| member(ctx, &child_global, "childIndexed"))
            .expect("read child indexed instance");
        assert!(ctx
            .member_set(&parent_global, "childIndexed", child_indexed)
            .is_ok());
        let parent_checks = ctx
            .eval_in_realm(
                &parent_global,
                "parentRealmProbe instanceof HostRealmDerived && parentRealmProbe.marker===21 && parentRealmProbe.doubled===42 && !(childRealmProbe instanceof HostRealmDerived) && parentIndexed.length===1 && parentIndexed[0]===11 && !(childIndexed instanceof HostRealmIndexed) && (new HostRealmDerived(24)).doubled===48",
            )
            .unwrap_or_else(|_| panic!("parent native class checks threw"));
        assert!(matches!(parent_checks, Value::Bool(true)));
        assert!(ctx
            .with_instance::<HostRealmDerived, _>(&child_instance, |value| value.base.marker)
            .is_ok_and(|marker| marker == 23));
        assert_eq!(
            object_id(ctx, &ctx.global_object()),
            object_id(ctx, &parent_global)
        );
    }

    #[test]
    fn host_realm_scope_restores_after_js_and_op_errors_without_changing_thrown_identity() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        let parent_id = object_id(ctx, &ctx.global_object());
        let child = ctx.create_host_realm();
        let child_global = child.global();
        let thrown = Value::Obj(ctx.new_object());
        let thrown_id = object_id(ctx, &thrown);

        let js_result = ctx
            .with_host_realm(&child, |ctx| {
                assert!(ctx
                    .member_set(&child_global, "sentinel", thrown.clone())
                    .is_ok());
                ctx.eval_in_realm(&child_global, "throw globalThis.sentinel")
            })
            .expect("enter child realm");
        let js_thrown = match js_result {
            Err(abrupt) => abrupt_value(abrupt),
            Ok(_) => panic!("script should throw the sentinel"),
        };
        assert_eq!(object_id(ctx, &js_thrown), thrown_id);
        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);

        let op_result: OpResult<()> = Err(OpError::thrown(thrown.clone()));
        let scoped_result = ctx.with_host_realm(&child, |_ctx| op_result);
        let returned_thrown = match scoped_result {
            Ok(Err(error)) => error.to_value(ctx),
            Ok(Ok(())) => panic!("callback should return its OpError"),
            Err(error) => panic!("unexpected realm entry error: {error}"),
        };
        assert_eq!(object_id(ctx, &returned_thrown), thrown_id);
        assert_eq!(object_id(ctx, &ctx.global_object()), parent_id);
    }

    #[test]
    fn specification_function_templates_keep_realm_neutral_metadata_and_live_closure_owners() {
        let mut engine=Engine::new();
        let ctx=engine.ctx();
        let child=ctx.create_host_realm();
        let weak=ctx.weak_value(&child.global()).expect("child global");
        let function=ctx.eval_value_in_host_realm(&child,"(function held(){return 7})",false)
            .expect("enter child").ok().expect("child function");
        let ast={
            let object=function.as_obj().expect("function object").borrow();
            let crate::value::Callable::User(user)=&object.call else {panic!("user function")};
            user.func.clone()
        };
        let getter_id=|function:&Value| {
            let object=function.as_obj().expect("function object").borrow();
            object.props.get("arguments").expect("legacy arguments descriptor")
                .getter().and_then(Value::object_identity).expect("realm getter")
        };
        let child_getter=getter_id(&function);
        let parent=ctx.make_function(ast.clone(),ctx.global_env.clone());
        let parent_getter=getter_id(&parent);
        assert_ne!(child_getter,parent_getter,"shared AST must instantiate the closure's actual realm descriptors");
        assert_eq!(Some(parent_getter),ctx.extra_protos.get(crate::bytecode::reflect::ARGUMENTS_GETTER).map(|getter|crate::value::Gc::as_ptr(getter) as usize));
        let maps=ast.fn_maps.get().expect("cached maps");
        let mut edges=0;
        maps.fn_map.visit_object_refs(0,usize::MAX,&mut |_|edges+=1);
        if let Some(map)=&maps.eager_map {map.visit_object_refs(0,usize::MAX,&mut |_|edges+=1);}
        if let Some(map)=&maps.proto_map {map.visit_object_refs(0,usize::MAX,&mut |_|edges+=1);}
        if let Some(map)=maps.named.get() {map.1.visit_object_refs(0,usize::MAX,&mut |_|edges+=1);}
        assert_eq!(edges,0,"AST templates own only neutral metadata, never realm JS Values");
        ctx.dispose_host_realm(&child).expect("dispose inactive realm");
        drop(child);
        ctx.collect_garbage_for_host();
        assert!(weak.upgrade().is_some(),"an actual retained child closure preserves its origin realm");
        assert!(matches!(ctx.invoke(function.clone(),Value::Undefined,&[]),Ok(Value::Num(7.0))));
        drop(function);
        ctx.collect_garbage_for_host();
        assert!(weak.upgrade().is_none(),"external AST and parent closures cannot retain the retired child realm through cached descriptors");
        let again=ctx.make_function(ast.clone(),ctx.global_env.clone());
        assert_eq!(getter_id(&again),parent_getter,"externally retained AST remains reusable after its first realm is collected");
        assert!(matches!(ctx.invoke(again,Value::Undefined,&[]),Ok(Value::Num(7.0))));
        assert!(matches!(ctx.invoke(parent,Value::Undefined,&[]),Ok(Value::Num(7.0))));
    }

    #[test]
    fn specification_pending_task_exceptions_preserve_callback_global_until_delivery() {
        let mut engine = Engine::new();
        engine.report_task_errors();
        let child = engine.ctx().create_host_realm();
        let weak = engine.ctx().weak_value(&child.global()).expect("child global");
        let callback = engine.eval_value_in_host_realm(&child, "() => { throw 'primitive'; }", false)
            .expect("callback parses").unwrap_or_else(|error| match engine.describe_throw(error) {
                crate::Completion::Throw { name, message } => panic!("callback evaluation: {name}: {message}"),
                crate::Completion::Value(_) => panic!("callback evaluation failed without a thrown description"),
            });
        // queueMicrotask is a Runtime-installed global, not an ECMAScript
        // Engine intrinsic. Use the same canonical queue entry that it calls.
        engine.ctx().queue_microtask(callback.clone());
        engine.run_microtasks();
        engine.ctx().dispose_host_realm(&child).expect("retire inactive child realm");
        drop((child, callback));
        engine.collect_garbage();
        assert!(weak.upgrade().is_some(), "pending primitive exceptions retain their actual reporting global");
        let errors = engine.take_task_errors_with_globals();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].1.object_identity(), weak.upgrade().and_then(|global| global.object_identity()));
        assert!(matches!(&errors[0].0, Value::Str(value) if value.as_str() == "primitive"));
        drop(errors);
        engine.collect_garbage();
        assert!(weak.upgrade().is_none(), "draining pending reports releases their transient source ownership");
        let legacy = engine.eval_value("() => { throw 7; }").expect("legacy callback parses")
            .unwrap_or_else(|error| match engine.describe_throw(error) {
                crate::Completion::Throw { name, message } => panic!("legacy callback evaluation: {name}: {message}"),
                crate::Completion::Value(_) => panic!("legacy callback evaluation failed without a thrown description"),
            });
        engine.ctx().queue_microtask(legacy);
        engine.run_microtasks();
        assert!(matches!(engine.take_task_errors().as_slice(), [Value::Num(7.0)]));
    }

    #[test]
    fn host_realm_scope_restores_and_persists_during_unwind_and_rejects_other_engines() {
        let mut first = Engine::new();
        let first_ctx = first.ctx();
        let parent_id = object_id(first_ctx, &first_ctx.global_object());
        let child = first_ctx.create_host_realm();
        let child_global = child.global();

        let panic_result = catch_unwind(AssertUnwindSafe(|| {
            let _ = first_ctx.with_host_realm(&child, |ctx| {
                assert!(ctx
                    .member_set(&child_global, "beforePanic", Value::Num(7.0))
                    .is_ok());
                panic!("host callback panic");
            });
        }));
        assert!(panic_result.is_err());
        assert_eq!(object_id(first_ctx, &first_ctx.global_object()), parent_id);
        let persisted = first_ctx
            .with_host_realm(&child, |ctx| member(ctx, &child_global, "beforePanic"))
            .expect("child realm survives unwind");
        assert!(matches!(persisted, Value::Num(value) if value == 7.0));

        let mut second = Engine::new();
        let second_ctx = second.ctx();
        let second_parent_id = object_id(second_ctx, &second_ctx.global_object());
        assert_eq!(
            second_ctx.with_host_realm(&child, |_| ()),
            Err(HostRealmScopeError::DifferentInterpreter)
        );
        assert_eq!(
            object_id(second_ctx, &second_ctx.global_object()),
            second_parent_id
        );
    }
}
