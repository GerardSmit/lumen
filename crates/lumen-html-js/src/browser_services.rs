//! Host-backed browser services whose authority and data are supplied by the embedder.
//!
//! This module intentionally does not emulate a clipboard. Without host callbacks the API
//! rejects, so page code cannot mistake an empty fake clipboard for the system clipboard.
use super::*;
use lumen::embed::{Deferred, JsObject, Promise};
use lumen_bind::{IntoError, This};
use lumen_host::navigator::Navigator;
use std::{
    cell::Cell,
    rc::Weak,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardOperation {
    ReadText,
    WriteText,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClipboardPermission {
    Granted,
    Prompt,
    Denied,
}

/// Explicit callbacks supplied by a browser host. They must perform a real clipboard action.
#[derive(Clone)]
pub struct ClipboardHost {
    pub permission: Rc<dyn Fn(ClipboardOperation, bool) -> ClipboardPermission>,
    pub read_text: Rc<dyn Fn() -> Result<String, String>>,
    pub write_text: Rc<dyn Fn(&str) -> Result<(), String>>,
}

/// Per-realm clipboard hooks and the trusted activation mark maintained by host input dispatch.
#[derive(Default)]
pub struct RealmBrowserServices {
    clipboard: RefCell<Option<ClipboardHost>>,
    pub(super) presentation: super::presentation::RealmPresentation,
    pub(super) notifications: super::notifications::RealmNotifications,
    last_user_activation: Cell<Option<Instant>>,
    has_been_active: Cell<bool>,
    activation_generation: Cell<u64>,
    activation_expiry_notified_generation: Cell<u64>,
    permission_statuses: RefCell<Vec<Weak<PermissionStatusData>>>,
}

impl RealmBrowserServices {
    fn set_clipboard_host(&self, host: Option<ClipboardHost>) {
        *self.clipboard.borrow_mut() = host;
    }

    fn mark_user_activation(&self) {
        self.last_user_activation.set(Some(Instant::now()));
        self.has_been_active.set(true);
        self.activation_generation
            .set(self.activation_generation.get().saturating_add(1));
    }

    fn has_transient_user_activation(&self) -> bool {
        self.last_user_activation
            .get()
            .is_some_and(|when| when.elapsed() < Duration::from_secs(5))
    }

    fn clipboard_host(&self) -> Option<ClipboardHost> {
        self.clipboard.borrow().clone()
    }

    fn has_live_activation_statuses(&self) -> bool {
        let mut statuses = self.permission_statuses.borrow_mut();
        let mut has_dynamic = false;
        statuses.retain(|status| {
            let Some(status) = status.upgrade() else {
                return false;
            };
            has_dynamic |= status.activation_override.is_none();
            true
        });
        has_dynamic
    }

    fn browser_services_delay_ms(&self) -> Option<u64> {
        if !self.has_live_activation_statuses() {
            return None;
        }
        let generation = self.activation_generation.get();
        if generation == 0 || generation == self.activation_expiry_notified_generation.get() {
            return None;
        }
        let activated_at = self.last_user_activation.get()?;
        let expiry = activated_at + Duration::from_secs(5);
        let remaining = expiry.saturating_duration_since(Instant::now());
        let milliseconds = remaining.as_millis();
        let rounded_up = milliseconds + u128::from(remaining.subsec_nanos() % 1_000_000 != 0);
        Some(rounded_up as u64)
    }

    fn take_expired_activation(&self) -> bool {
        let generation = self.activation_generation.get();
        if generation == 0
            || generation == self.activation_expiry_notified_generation.get()
            || self.has_transient_user_activation()
        {
            return false;
        }
        self.activation_expiry_notified_generation.set(generation);
        true
    }

    fn has_been_active(&self) -> bool {
        self.has_been_active.get()
    }

    fn permission_state(
        &self,
        operation: ClipboardOperation,
        activation: bool,
    ) -> ClipboardPermission {
        self.clipboard
            .borrow()
            .as_ref()
            .map_or(ClipboardPermission::Denied, |host| {
                (host.permission)(operation, activation)
            })
    }

    pub(super) fn refresh_notification_permission_statuses(
        &self,
        ctx: &mut Ctx,
        realm: &DomRealm,
    ) -> OpResult<()> {
        let statuses = {
            let mut statuses = self.permission_statuses.borrow_mut();
            let live = statuses
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            statuses.retain(|status| status.strong_count() != 0);
            live
        };
        for status in statuses {
            if status.notification {
                status.refresh(ctx, realm)?;
            }
        }
        Ok(())
    }
}

impl DomRealm {
    /// Replace the explicit host clipboard callbacks. Passing `None` disables clipboard access.
    pub fn set_clipboard_host(&self, host: Option<ClipboardHost>) {
        self.browser_services.set_clipboard_host(host);
    }

    /// Mark a real host-dispatched user activation. Script-created events must never call this.
    pub fn mark_user_activation(&self) {
        self.browser_services.mark_user_activation();
    }

    pub fn has_transient_user_activation(&self) -> bool {
        self.browser_services.has_transient_user_activation()
    }

    /// Consume authority for APIs that open a native user interface.
    pub fn consume_user_activation(&self) -> bool {
        let active = self.has_transient_user_activation();
        self.browser_services.last_user_activation.set(None);
        active
    }

    pub fn has_been_active(&self) -> bool {
        self.browser_services.has_been_active()
    }

    /// Milliseconds until activation-dependent PermissionStatus objects must be refreshed.
    /// Returns `None` when there is no pending activation expiry to service.
    pub fn browser_services_delay_ms(&self) -> Option<u64> {
        self.browser_services.browser_services_delay_ms()
    }

    /// Pump time-based browser-service transitions. The embedder calls this on its host loop.
    pub fn pump_browser_services(&self, ctx: &mut Ctx) -> OpResult<()> {
        if self.browser_services.take_expired_activation() {
            self.notify_clipboard_permissions_changed(ctx)?;
        }
        super::notifications::pump_permission_origin(ctx, self)?;
        Ok(())
    }

    /// Re-evaluate retained permission objects after a host policy or activation change.
    pub fn notify_clipboard_permissions_changed(&self, ctx: &mut Ctx) -> OpResult<()> {
        let statuses = {
            let mut statuses = self.browser_services.permission_statuses.borrow_mut();
            let live = statuses
                .iter()
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            statuses.retain(|status| status.strong_count() != 0);
            live
        };
        for status in statuses {
            status.refresh(ctx, self)?;
        }
        Ok(())
    }
}

#[lumen_bind::class(name = "Navigator", extends = Navigator, hint(js(webidl)))]
pub struct DomNavigator {
    base: Navigator,
    clipboard: Value,
    permissions: Value,
    user_activation: Value,
}

#[lumen_bind::methods]
impl DomNavigator {
    #[getter]
    fn clipboard(&self) -> Value {
        self.clipboard.clone()
    }

    #[getter]
    fn permissions(&self) -> Value {
        self.permissions.clone()
    }

    #[getter]
    fn user_activation(&self) -> Value {
        self.user_activation.clone()
    }
}

#[lumen_bind::class(name = "UserActivation", hint(js(webidl)))]
pub struct DomUserActivation {
    realm: Weak<DomRealm>,
}

#[lumen_bind::methods]
impl DomUserActivation {
    #[getter]
    fn is_active(&self) -> bool {
        self.realm
            .upgrade()
            .is_some_and(|realm| realm.has_transient_user_activation())
    }

    #[getter]
    fn has_been_active(&self) -> bool {
        self.realm
            .upgrade()
            .is_some_and(|realm| realm.has_been_active())
    }
}

#[lumen_bind::class(name = "Permissions", hint(js(webidl)))]
pub struct DomPermissions {
    realm: Weak<DomRealm>,
}

#[lumen_bind::methods]
impl DomPermissions {
    fn query(&self, ctx: &mut Ctx, descriptor: Value) -> Promise<Value> {
        let result = (|| {
            let realm = self.realm.upgrade().ok_or_else(|| {
                OpError::new(
                    "InvalidStateError",
                    "the permissions realm has been destroyed",
                )
            })?;
            let name = ctx
                .member_get(&descriptor, "name")
                .map_err(OpError::thrown)?;
            if matches!(name, Value::Undefined | Value::Null) {
                return Err(OpError::new(
                    "TypeError",
                    "a permission descriptor requires a name",
                ));
            }
            let name = ctx
                .coerce_string(&name)
                .map_err(OpError::thrown)?
                .to_string();
            let (operation, activation_override) = match name.as_str() {
                "clipboard-read" => (ClipboardOperation::ReadText, None),
                "clipboard-write" => {
                    let allow_without_gesture = ctx
                        .member_get(&descriptor, "allowWithoutGesture")
                        .map_err(OpError::thrown)?;
                    let allow_without_gesture = ctx.to_boolean(&allow_without_gesture);
                    (
                        ClipboardOperation::WriteText,
                        allow_without_gesture.then_some(false),
                    )
                }
                "notifications" => {
                    let data = Rc::new(PermissionStatusData {
                        operation: ClipboardOperation::ReadText,
                        activation_override: Some(false),
                        name,
                        state: Cell::new(ClipboardPermission::Denied),
                        notification: true,
                        notification_state: Cell::new(realm.notification_permission()),
                        target: DomEventTarget::independent(&realm),
                        wrapper: RefCell::new(None),
                    });
                    realm
                        .browser_services
                        .permission_statuses
                        .borrow_mut()
                        .push(Rc::downgrade(&data));
                    return Ok(ctx.new_instance(DomPermissionStatus {
                        base: DomEventTarget::from_data(data.target.data_handle()),
                        data,
                    }));
                }
                _ => {
                    return Err(OpError::new(
                        "TypeError",
                        format!("unsupported permission descriptor: {name}"),
                    ));
                }
            };
            let activation = activation_override
                .unwrap_or_else(|| realm.browser_services.has_transient_user_activation());
            let state = realm
                .browser_services
                .permission_state(operation, activation);
            let data = Rc::new(PermissionStatusData {
                operation,
                activation_override,
                name,
                state: Cell::new(state),
                notification: false,
                notification_state: Cell::new(super::notifications::NotificationPermission::Denied),
                target: DomEventTarget::independent(&realm),
                wrapper: RefCell::new(None),
            });
            realm
                .browser_services
                .permission_statuses
                .borrow_mut()
                .push(Rc::downgrade(&data));
            Ok(ctx.new_instance(DomPermissionStatus {
                base: DomEventTarget::from_data(data.target.data_handle()),
                data,
            }))
        })();
        let status = match result {
            Ok(status) => status,
            Err(error) => return Promise::rejected(error),
        };
        let deferred = Deferred::new(ctx);
        let promise = Promise::pending(&deferred);
        match scheduling::queue_task(ctx, move |ctx| {
            deferred.resolve(ctx, status);
            Ok(())
        }) {
            Ok(()) => promise,
            Err(error) => Promise::rejected(error),
        }
    }
}

struct PermissionStatusData {
    operation: ClipboardOperation,
    activation_override: Option<bool>,
    name: String,
    state: Cell<ClipboardPermission>,
    notification: bool,
    notification_state: Cell<super::notifications::NotificationPermission>,
    target: DomEventTarget,
    wrapper: RefCell<Option<WeakValue>>,
}

impl PermissionStatusData {
    fn refresh(&self, ctx: &mut Ctx, realm: &DomRealm) -> OpResult<()> {
        if self.notification {
            let next = realm.notification_permission();
            let previous = self.notification_state.replace(next);
            if previous == next {
                return Ok(());
            }
            return self.dispatch_change(ctx);
        }
        let activation = self
            .activation_override
            .unwrap_or_else(|| realm.browser_services.has_transient_user_activation());
        let next = realm
            .browser_services
            .permission_state(self.operation, activation);
        let previous = self.state.replace(next);
        if previous == next {
            return Ok(());
        }
        self.dispatch_change(ctx)
    }

    fn dispatch_change(&self, ctx: &mut Ctx) -> OpResult<()> {
        let Some(wrapper) = self.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) else {
            return Ok(());
        };
        // Retain the receiver until its permissions task runs, separately from
        // Promise reactions and other statuses updated by the same host call.
        scheduling::queue_task(ctx, move |ctx| {
            let options = Value::Obj(ctx.new_object());
            ctx.set_member(&options, "bubbles", Value::Bool(false))
                .and_then(|_| ctx.set_member(&options, "cancelable", Value::Bool(false)))
                .map_err(|_| OpError::new("TypeError", "permission change event setup failed"))?;
            let event = DomEvent::new(ctx, "change", Some(options))?;
            let event = ctx.new_instance(event);
            let event = JsObject::from_value(event)
                .ok_or_else(|| OpError::new("TypeError", "permission change event is invalid"))?;
            super::events::dispatch_user_agent_event(ctx, This(wrapper), event)?;
            Ok(())
        })
    }
}

#[lumen_bind::class(name = "PermissionStatus", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomPermissionStatus {
    base: DomEventTarget,
    data: Rc<PermissionStatusData>,
}

#[lumen_bind::methods]
impl DomPermissionStatus {
    #[getter]
    fn name(&self) -> String {
        self.data.name.clone()
    }

    #[getter]
    fn state(&self) -> &'static str {
        match self.data.state.get() {
            _ if self.data.notification => match self.data.notification_state.get() {
                super::notifications::NotificationPermission::Default => "prompt",
                super::notifications::NotificationPermission::Granted => "granted",
                super::notifications::NotificationPermission::Denied => "denied",
            },
            ClipboardPermission::Granted => "granted",
            ClipboardPermission::Prompt => "prompt",
            ClipboardPermission::Denied => "denied",
        }
    }

    fn add_event_listener(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
        crate::events::add_event_listener(ctx, this, kind, callback, options)
    }

    #[getter]
    fn onchange(&self) -> Nullable<lumen::embed::JsFunction> {
        Nullable(self.base.handler("change"))
    }

    #[setter]
    fn set_onchange(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "change", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

#[lumen_bind::class(name = "Clipboard", hint(js(webidl)))]
pub struct DomClipboard {
    realm: Weak<DomRealm>,
}

#[lumen_bind::methods]
impl DomClipboard {
    fn read_text(&self) -> Promise<String> {
        Promise::ready(self.run(ClipboardOperation::ReadText, |host| {
            (host.read_text)().map_err(|message| {
                OpError::new("UnknownError", format!("clipboard read failed: {message}"))
            })
        }))
    }

    fn write_text(&self, text: &str) -> Promise<()> {
        Promise::ready(self.run(ClipboardOperation::WriteText, |host| {
            (host.write_text)(text).map_err(|message| {
                OpError::new("UnknownError", format!("clipboard write failed: {message}"))
            })
        }))
    }
}

impl DomClipboard {
    fn run<T>(
        &self,
        operation: ClipboardOperation,
        action: impl FnOnce(&ClipboardHost) -> OpResult<T>,
    ) -> OpResult<T> {
        let realm = self.realm.upgrade().ok_or_else(|| {
            OpError::new(
                "InvalidStateError",
                "the clipboard realm has been destroyed",
            )
        })?;
        let host = realm.browser_services.clipboard_host().ok_or_else(|| {
            OpError::new(
                "NotSupportedError",
                "the host has not supplied clipboard access",
            )
        })?;
        let activation = realm.browser_services.has_transient_user_activation();
        match (host.permission)(operation, activation) {
            ClipboardPermission::Granted => action(&host),
            ClipboardPermission::Prompt | ClipboardPermission::Denied => Err(OpError::new(
                "NotAllowedError",
                "clipboard permission was not granted by the host",
            )),
        }
    }
}

/// Install a typed `navigator.clipboard` backed by weak ownership of this DOM realm.
pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    super::presentation::install(realm);
    let global = ctx.global_object();
    let existing = ctx
        .member_get(&global, "navigator")
        .map_err(OpError::thrown)?;
    let _ = ctx.class_constructor::<DomClipboard>();
    let _ = ctx.class_constructor::<DomPermissions>();
    let _ = ctx.class_constructor::<DomPermissionStatus>();
    let _ = ctx.class_constructor::<DomUserActivation>();
    let clipboard = ctx.new_instance(DomClipboard {
        realm: Rc::downgrade(realm),
    });
    let permissions = ctx.new_instance(DomPermissions {
        realm: Rc::downgrade(realm),
    });
    let user_activation = ctx.new_instance(DomUserActivation {
        realm: Rc::downgrade(realm),
    });
    let navigator_data = DomNavigator {
        base: Navigator,
        clipboard,
        permissions,
        user_activation,
    };
    let navigator = match existing {
        Value::Obj(_) => {
            ctx.attach_instance(&existing, navigator_data)
                .map_err(|error| error.into_error(ctx))?;
            existing
        }
        _ => ctx.new_instance(navigator_data),
    };
    let navigator_constructor = ctx.class_constructor::<DomNavigator>();
    let clipboard_constructor = ctx.class_constructor::<DomClipboard>();
    let permissions_constructor = ctx.class_constructor::<DomPermissions>();
    let status_constructor = ctx.class_constructor::<DomPermissionStatus>();
    let activation_constructor = ctx.class_constructor::<DomUserActivation>();
    crate::install_interface(ctx, &global, "Navigator", navigator_constructor)
        .and_then(|_| crate::install_interface(ctx, &global, "Clipboard", clipboard_constructor))
        .and_then(|_| {
            crate::install_interface(ctx, &global, "Permissions", permissions_constructor)
        })
        .and_then(|_| {
            crate::install_interface(ctx, &global, "PermissionStatus", status_constructor)
        })
        .and_then(|_| {
            crate::install_interface(ctx, &global, "UserActivation", activation_constructor)
        })
        .and_then(|_| ctx.member_set(&global, "navigator", navigator))
        .map_err(|_| OpError::new("Error", "navigator clipboard installation failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).unwrap() {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "stack")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(text) => Some(text.as_str().to_owned()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "JavaScript error without a stack".into());
                panic!("{message}\nSource: {source}");
            }
        }
    }

    fn install_engine() -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        let realm = super::super::install(engine.ctx(), "<main></main>", 32).unwrap();
        install(engine.ctx(), &realm).unwrap();
        (engine, realm)
    }

    #[test]
    fn clipboard_promises_roundtrip_real_mock_host_data_after_activation() {
        let (mut engine, realm) = install_engine();
        let clipboard = Rc::new(RefCell::new("host seed".to_owned()));
        let reads = Rc::new(Cell::new(0usize));
        let writes = Rc::new(Cell::new(0usize));
        let permission_calls = Rc::new(RefCell::new(Vec::new()));
        let read_state = clipboard.clone();
        let read_count = reads.clone();
        let write_state = clipboard.clone();
        let write_count = writes.clone();
        let permission_log = permission_calls.clone();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(move |operation, activated| {
                permission_log.borrow_mut().push(operation);
                if activated {
                    ClipboardPermission::Granted
                } else {
                    ClipboardPermission::Prompt
                }
            }),
            read_text: Rc::new(move || {
                read_count.set(read_count.get() + 1);
                Ok(read_state.borrow().clone())
            }),
            write_text: Rc::new(move |text| {
                write_count.set(write_count.get() + 1);
                *write_state.borrow_mut() = text.to_owned();
                Ok(())
            }),
        }));
        assert!(matches!(
            eval(
                &mut engine,
                "var deniedRead='pending'; navigator.clipboard.readText().then(value=>deniedRead=value, error=>deniedRead=error.name); navigator.clipboard instanceof Clipboard && navigator instanceof Navigator"
            ),
            Value::Bool(true)
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(
            matches!(eval(&mut engine, "deniedRead"), Value::Str(value) if value.as_str() == "NotAllowedError")
        );
        assert_eq!(reads.get(), 0);

        realm.mark_user_activation();
        assert!(matches!(
            eval(
                &mut engine,
                "var clipboardResult='pending'; navigator.clipboard.readText().then(value=>clipboardResult=value); navigator.clipboard.writeText('host write').then(()=>clipboardResult+=':written'); navigator.clipboard.readText().then(value=>clipboardResult+=':'+value); void 0"
            ),
            Value::Undefined
        ));
        engine.ctx().drain_microtasks_for_host();
        assert!(
            matches!(eval(&mut engine, "clipboardResult"), Value::Str(value) if value.as_str() == "host seed:written:host write")
        );
        assert_eq!(reads.get(), 2);
        assert_eq!(writes.get(), 1);
        assert_eq!(&*clipboard.borrow(), "host write");
        assert_eq!(
            &*permission_calls.borrow(),
            &[
                ClipboardOperation::ReadText,
                ClipboardOperation::ReadText,
                ClipboardOperation::WriteText,
                ClipboardOperation::ReadText,
            ]
        );
    }

    #[test]
    fn clipboard_denial_missing_host_and_host_failures_reject_promises() {
        let (mut engine, realm) = install_engine();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(|_, _| ClipboardPermission::Denied),
            read_text: Rc::new(|| Ok("must not read".into())),
            write_text: Rc::new(|_| Ok(())),
        }));
        eval(
            &mut engine,
            "var denied=''; navigator.clipboard.writeText('no').catch(error=>denied=error.name)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(
            matches!(eval(&mut engine, "denied"), Value::Str(value) if value.as_str() == "NotAllowedError")
        );

        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(|_, _| ClipboardPermission::Granted),
            read_text: Rc::new(|| Err("backend offline".into())),
            write_text: Rc::new(|_| Err("backend offline".into())),
        }));
        eval(
            &mut engine,
            "var errors=[]; navigator.clipboard.readText().catch(error=>errors.push(error.name)); navigator.clipboard.writeText('x').catch(error=>errors.push(error.name))",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(
            matches!(eval(&mut engine, "errors.join(',')"), Value::Str(value) if value.as_str() == "UnknownError,UnknownError")
        );

        realm.set_clipboard_host(None);
        eval(
            &mut engine,
            "var missing=''; navigator.clipboard.readText().catch(error=>missing=error.name)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(
            matches!(eval(&mut engine, "missing"), Value::Str(value) if value.as_str() == "NotSupportedError")
        );
    }

    #[test]
    fn clipboard_permission_statuses_are_typed_and_notify_state_changes() {
        let (mut engine, realm) = install_engine();
        let permission = Rc::new(Cell::new(ClipboardPermission::Prompt));
        let permission_state = permission.clone();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(move |_, _| permission_state.get()),
            read_text: Rc::new(|| Ok(String::new())),
            write_text: Rc::new(|_| Ok(())),
        }));
        eval(
            &mut engine,
            "var status, changes=0, queryError=''; navigator.permissions.query({name:'clipboard-read'}).then(value=>{status=value;status.onchange=()=>changes++},error=>queryError=error.name)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "navigator.permissions instanceof Permissions && status instanceof PermissionStatus && status instanceof EventTarget && status.name==='clipboard-read' && status.state==='prompt' && queryError===''"
            ),
            Value::Bool(true)
        ));

        permission.set(ClipboardPermission::Granted);
        realm
            .notify_clipboard_permissions_changed(engine.ctx())
            .unwrap();
        assert!(matches!(
            eval(&mut engine, "status.state==='granted' && changes===0"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "status.state==='granted' && changes===1"),
            Value::Bool(true)
        ));

        eval(
            &mut engine,
            "var unsupported='', getterError=''; navigator.permissions.query({name:'camera'}).catch(error=>unsupported=error.name); navigator.permissions.query(Object.defineProperty({},'name',{get(){throw new Error('descriptor getter')}})).catch(error=>getterError=error.message)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(
                &mut engine,
                "unsupported==='TypeError' && getterError==='descriptor getter'"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn permission_queries_resolve_on_tasks_and_nested_queries_wait_for_next_turn() {
        let (mut engine, realm) = install_engine();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(|_, _| ClipboardPermission::Granted),
            read_text: Rc::new(|| Ok(String::new())),
            write_text: Rc::new(|_| Ok(())),
        }));
        eval(&mut engine, "var queryOrder=[]; navigator.permissions.query({name:'clipboard-read'}).then(status=>{queryOrder.push(status instanceof PermissionStatus && status.state==='granted' ? 'query' : 'bad');navigator.permissions.query({name:'clipboard-write'}).then(()=>queryOrder.push('nested'))});Promise.resolve().then(()=>queryOrder.push('microtask'))");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(&mut engine, "queryOrder.join(',')==='microtask'"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::task_pending(engine.ctx()));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "queryOrder.join(',')==='microtask,query'"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::task_pending(engine.ctx()));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "queryOrder.join(',')==='microtask,query,nested'"
            ),
            Value::Bool(true)
        ));
        assert!(!super::scheduling::task_pending(engine.ctx()));
    }

    #[test]
    fn permission_changes_are_trusted_tasks_with_microtask_checkpoints() {
        let (mut engine, realm) = install_engine();
        let permission = Rc::new(Cell::new(ClipboardPermission::Prompt));
        let permission_state = permission.clone();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(move |_, _| permission_state.get()),
            read_text: Rc::new(|| Ok(String::new())),
            write_text: Rc::new(|_| Ok(())),
        }));
        eval(&mut engine, "var firstStatus,secondStatus,permissionOrder=[]; navigator.permissions.query({name:'clipboard-read'}).then(status=>{firstStatus=status;status.onchange=event=>{permissionOrder.push(event.isTrusted && !event.bubbles && !event.cancelable && event.target===status ? 'first' : 'bad');Promise.resolve().then(()=>permissionOrder.push('checkpoint'));throw new Error('listener failure')}}); navigator.permissions.query({name:'clipboard-write'}).then(status=>{secondStatus=status;status.addEventListener('change',event=>permissionOrder.push(event.isTrusted && event.currentTarget===status ? 'second' : 'bad'))})");
        engine.ctx().drain_microtasks_for_host();
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        permission.set(ClipboardPermission::Granted);
        realm
            .notify_clipboard_permissions_changed(engine.ctx())
            .unwrap();
        eval(
            &mut engine,
            "Promise.resolve().then(()=>permissionOrder.push('before'))",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(eval(&mut engine, "firstStatus.state==='granted' && secondStatus.state==='granted' && permissionOrder.join(',')==='before'"), Value::Bool(true)));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "permissionOrder.join(',')==='before,first,checkpoint,second'"
            ),
            Value::Bool(true)
        ));
        realm
            .notify_clipboard_permissions_changed(engine.ctx())
            .unwrap();
        assert!(!super::scheduling::task_pending(engine.ctx()));
    }

    #[test]
    fn navigator_user_activation_reflects_only_real_realm_marks() {
        let (mut engine, realm) = install_engine();
        assert!(matches!(
            eval(
                &mut engine,
                "navigator.userActivation instanceof UserActivation && navigator.userActivation===navigator.userActivation && !navigator.userActivation.isActive && !navigator.userActivation.hasBeenActive"
            ),
            Value::Bool(true)
        ));
        realm.mark_user_activation();
        assert!(matches!(
            eval(
                &mut engine,
                "navigator.userActivation.isActive && navigator.userActivation.hasBeenActive"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn permission_activation_expiry_is_pumped_once_without_losing_sticky_activation() {
        let (mut engine, realm) = install_engine();
        realm.set_clipboard_host(Some(ClipboardHost {
            permission: Rc::new(|_, activated| {
                if activated {
                    ClipboardPermission::Granted
                } else {
                    ClipboardPermission::Prompt
                }
            }),
            read_text: Rc::new(|| Ok(String::new())),
            write_text: Rc::new(|_| Ok(())),
        }));
        realm.mark_user_activation();
        eval(
            &mut engine,
            "var status, changes=0; navigator.permissions.query({name:'clipboard-read'}).then(value=>{status=value;status.onchange=()=>changes++})",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "status.state==='granted'"),
            Value::Bool(true)
        ));

        realm
            .browser_services
            .last_user_activation
            .set(Some(Instant::now() - Duration::from_secs(6)));
        assert_eq!(realm.browser_services_delay_ms(), Some(0));
        realm.pump_browser_services(engine.ctx()).unwrap();
        assert!(matches!(
            eval(&mut engine, "status.state==='prompt' && changes===0"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "status.state==='prompt' && changes===1 && !navigator.userActivation.isActive && navigator.userActivation.hasBeenActive"
            ),
            Value::Bool(true)
        ));
        assert_eq!(realm.browser_services_delay_ms(), None);
        realm.pump_browser_services(engine.ctx()).unwrap();
        assert!(matches!(
            eval(&mut engine, "changes===1"),
            Value::Bool(true)
        ));
    }
}
