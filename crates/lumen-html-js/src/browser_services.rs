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
    pub(super) render_capture: super::rendering_capture::RealmRenderCapture,
    pub(super) notifications: super::notifications::RealmNotifications,
    last_user_activation: Cell<Option<Instant>>,
    has_been_active: Cell<bool>,
    activation_generation: Cell<u64>,
    consumed_history_activation_generation: Cell<u64>,
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

    pub(crate) fn consume_history_action_activation(&self) -> bool {
        let generation = self.browser_services.activation_generation.get();
        let active = generation > self.browser_services.consumed_history_activation_generation.get();
        self.browser_services.consumed_history_activation_generation.set(generation);
        active
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

/// What `navigator` reports about the browser. The embedder replaces the default with
/// [`set_navigator_profile`] before installing documents.
#[derive(Clone, Debug)]
pub struct NavigatorProfile {
    pub user_agent: String,
    pub platform: String,
    pub vendor: String,
    pub language: String,
    pub languages: Vec<String>,
    pub hardware_concurrency: u32,
    pub max_touch_points: u32,
    pub cookie_enabled: bool,
    pub online: bool,
    pub pdf_viewer_enabled: bool,
}

impl Default for NavigatorProfile {
    fn default() -> Self {
        Self {
            user_agent: "Mozilla/5.0 (X11; Linux aarch64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Lumen".into(),
            platform: "Linux aarch64".into(),
            vendor: "Google Inc.".into(),
            language: "en-US".into(),
            languages: vec!["en-US".into(), "en".into()],
            hardware_concurrency: 4,
            max_touch_points: 0,
            cookie_enabled: true,
            online: true,
            pdf_viewer_enabled: true,
        }
    }
}

/// Set the `navigator` profile of documents installed afterwards in this interpreter.
pub fn set_navigator_profile(ctx: &mut Ctx, profile: NavigatorProfile) {
    ctx.op_state().put(profile);
}

#[lumen_bind::class(name = "Navigator", extends = Navigator, hint(js(webidl)))]
pub struct DomNavigator {
    base: Navigator,
    clipboard: Value,
    permissions: Value,
    user_activation: Value,
    languages: Value,
    plugins: Value,
    mime_types: Value,
    window_controls_overlay: Value,
    /// `fetch` as it was when the document was installed; page script cannot replace it.
    fetch: Value,
    beacon_bytes: Rc<Cell<usize>>,
    profile: NavigatorProfile,
    realm: Weak<DomRealm>,
}

/// Fetch's limit on the bodies of in-flight `keepalive` requests.
const KEEPALIVE_QUOTA: usize = 64 * 1024;

impl lumen::embed::NativeIdentityOwner for DomNavigator {
    const TRACES_NATIVE_VALUES: bool=true;
    fn trace_native_identities(&self,_:u64,_:&mut dyn FnMut(&Value)){}
    fn trace_native_values(&self,visit:&mut dyn FnMut(&Value)) {
        visit(&self.clipboard);visit(&self.permissions);visit(&self.user_activation);visit(&self.languages);visit(&self.fetch);visit(&self.plugins);visit(&self.mime_types);visit(&self.window_controls_overlay);
    }
}

#[lumen_bind::methods]
impl DomNavigator {
    #[getter(name = "userAgent")]
    fn user_agent(&self) -> String {
        self.profile.user_agent.clone()
    }

    #[getter(name = "appVersion")]
    fn app_version(&self) -> String {
        let agent = &self.profile.user_agent;
        agent.strip_prefix("Mozilla/").unwrap_or(agent).to_owned()
    }

    #[getter(name = "appName")]
    fn app_name(&self) -> &'static str {
        "Netscape"
    }

    #[getter(name = "appCodeName")]
    fn app_code_name(&self) -> &'static str {
        "Mozilla"
    }

    #[getter]
    fn product(&self) -> &'static str {
        "Gecko"
    }

    #[getter(name = "productSub")]
    fn product_sub(&self) -> &'static str {
        "20030107"
    }

    #[getter]
    fn vendor(&self) -> String {
        self.profile.vendor.clone()
    }

    #[getter(name = "vendorSub")]
    fn vendor_sub(&self) -> &'static str {
        ""
    }

    #[getter]
    fn platform(&self) -> String {
        self.profile.platform.clone()
    }

    #[getter]
    fn language(&self) -> String {
        self.profile.language.clone()
    }

    #[getter]
    fn languages(&self) -> Value {
        self.languages.clone()
    }

    #[getter(name = "hardwareConcurrency")]
    fn hardware_concurrency(&self) -> u32 {
        self.profile.hardware_concurrency
    }

    #[getter(name = "maxTouchPoints")]
    fn max_touch_points(&self) -> u32 {
        self.profile.max_touch_points
    }

    #[getter(name = "cookieEnabled")]
    fn cookie_enabled(&self) -> bool {
        self.profile.cookie_enabled
    }

    #[getter(name = "onLine")]
    fn on_line(&self) -> bool {
        self.profile.online
    }

    #[getter]
    fn webdriver(&self) -> bool {
        false
    }

    #[getter(name = "pdfViewerEnabled")]
    fn pdf_viewer_enabled(&self) -> bool {
        self.profile.pdf_viewer_enabled
    }

    #[getter(name = "doNotTrack")]
    fn do_not_track(&self) -> Nullable<String> {
        Nullable(None)
    }

    #[method(name = "javaEnabled")]
    fn java_enabled(&self) -> bool {
        false
    }

    /// Queues a keepalive POST through the `fetch` captured at install. Per the Beacon API the
    /// request is `no-cors` unless the body's content type is not CORS-safelisted, always sends
    /// credentials, and is refused (`false`) when in-flight keepalive bodies would exceed 64 KiB.
    /// Nothing throws once the request is handed to fetch.
    #[method(name = "sendBeacon", coerce)]
    fn send_beacon(&self, ctx: &mut Ctx, url: &str, data: Option<Value>) -> OpResult<bool> {
        let base = self.realm.upgrade().map(|realm| realm.base_url());
        let parsed = lumen_common::url::parse(url, base.as_deref())
            .map_err(|_| OpError::type_error(format!("Invalid beacon URL '{url}'")))?;
        if !matches!(parsed.scheme.as_str(), "http" | "https") {
            return Err(OpError::type_error("sendBeacon only supports http and https URLs"));
        }
        let data = data.filter(|data| !matches!(data, Value::Undefined | Value::Null));
        let (size, cors) = match &data {
            Some(data) => {
                let body = lumen_host::net::extract_body(ctx, data)?;
                let safelisted = body
                    .content_type
                    .as_deref()
                    .is_none_or(|kind| lumen_common::cors::is_no_cors_safelisted_header("content-type", kind));
                (body.bytes.len(), !safelisted)
            }
            None => (0, false),
        };
        if size > KEEPALIVE_QUOTA.saturating_sub(self.beacon_bytes.get()) {
            return Ok(false);
        }
        let init = Value::Obj(ctx.new_object());
        for (name, value) in [
            ("method", Value::str("POST")),
            ("keepalive", Value::Bool(true)),
            ("credentials", Value::str("include")),
            ("mode", Value::str(if cors { "cors" } else { "no-cors" })),
        ] {
            ctx.member_set(&init, name, value).map_err(OpError::thrown)?;
        }
        if let Some(data) = data {
            ctx.member_set(&init, "body", data).map_err(OpError::thrown)?;
        }
        let Ok(pending) = ctx.invoke(self.fetch.clone(), Value::Undefined, &[Value::from_string(url.to_owned()), init]) else {
            return Ok(false);
        };
        self.beacon_bytes.set(self.beacon_bytes.get() + size);
        let in_flight = self.beacon_bytes.clone();
        let settled = ctx.new_native_fn(
            "",
            1,
            Rc::new(move |_, _, _| {
                in_flight.set(in_flight.get().saturating_sub(size));
                Ok(Value::Undefined)
            }),
        );
        ctx.then_value(pending, settled.clone(), settled);
        Ok(true)
    }

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

    #[getter]
    fn plugins(&self) -> Value {
        self.plugins.clone()
    }

    #[getter(name = "mimeTypes")]
    fn mime_types(&self) -> Value {
        self.mime_types.clone()
    }

    #[getter(name = "windowControlsOverlay")]
    fn window_controls_overlay(&self) -> Value {
        self.window_controls_overlay.clone()
    }

    #[method(name = "registerProtocolHandler", coerce)]
    fn register_protocol_handler(&self, scheme: &str, url: &str) -> OpResult<()> {
        self.check_protocol_handler(scheme, url)
    }

    #[method(name = "unregisterProtocolHandler", coerce)]
    fn unregister_protocol_handler(&self, scheme: &str, url: &str) -> OpResult<()> {
        self.check_protocol_handler(scheme, url)
    }
}

impl DomNavigator {
    fn check_protocol_handler(&self, scheme: &str, url: &str) -> OpResult<()> {
        let realm = self
            .realm
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "the navigator realm has been destroyed"))?;
        crate::navigator_extras::check_protocol_handler(&realm, scheme, url)
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
    fn onchange(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "change")
    }

    #[setter]
    fn set_onchange(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        callback: crate::events::EventHandler,
    ) {
        self.base.set_event_handler(ctx, &this.0, "change", callback);
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
    let existing = match ctx.native_private_value_slot(&global, window_globals::NAVIGATOR_OWNER) {
        Some(original)=>original,
        None=>ctx.member_get(&global,"navigator").map_err(OpError::thrown)?,
    };
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
    let profile = ctx.op_state().get::<NavigatorProfile>().cloned().unwrap_or_default();
    let languages = ctx.make_array(profile.languages.iter().map(|language| Value::str(language)).collect());
    ctx.freeze_native_object(&languages);
    let fetch = match ctx.member_get(&global, "fetch") {
        Ok(fetch) if fetch.is_callable() => fetch,
        _ => ctx.op_function::<lumen_host::net::fetch_bindings::fetch::Op>(),
    };
    let (plugins, mime_types) = crate::navigator_extras::new_plugin_arrays(ctx, profile.pdf_viewer_enabled)?;
    let navigator_data = DomNavigator {
        base: Navigator,
        clipboard,
        permissions,
        user_activation,
        languages,
        plugins,
        mime_types,
        window_controls_overlay: crate::navigator_extras::new_window_controls_overlay(ctx),
        fetch,
        beacon_bytes: Rc::new(Cell::new(0)),
        profile,
        realm: Rc::downgrade(realm),
    };
    let navigator = match existing {
        Value::Obj(_) => {
            ctx.attach_instance(&existing, navigator_data)
                .map_err(|error| error.into_error(ctx))?;
            existing
        }
        _ => ctx.new_instance(navigator_data),
    };
    ctx.set_native_identity_owner::<DomNavigator>(&navigator)?;
    ctx.set_native_internal_value_slot(&global, window_globals::NAVIGATOR_OWNER, navigator.clone())
        .map_err(OpError::thrown)?;
    let navigator_constructor = ctx.class_constructor::<DomNavigator>();
    for (name, constructor) in crate::navigator_extras::constructors(ctx) {
        crate::install_interface(ctx, &global, name, constructor)
            .map_err(|_| OpError::new("Error", "navigator interface installation failed"))?;
    }
    let clipboard_constructor = ctx.class_constructor::<DomClipboard>();
    let permissions_constructor = ctx.class_constructor::<DomPermissions>();
    let status_constructor = ctx.class_constructor::<DomPermissionStatus>();
    let activation_constructor = ctx.class_constructor::<DomUserActivation>();
    crate::idl_shape::make_root_interface(ctx, &navigator_constructor, &clipboard_constructor)?;
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
            "var permissionStatus, changes=0, queryError=''; navigator.permissions.query({name:'clipboard-read'}).then(value=>{permissionStatus=value;permissionStatus.onchange=()=>changes++},error=>queryError=error.name)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "navigator.permissions instanceof Permissions && permissionStatus instanceof PermissionStatus && permissionStatus instanceof EventTarget && permissionStatus.name==='clipboard-read' && permissionStatus.state==='prompt' && queryError===''"
            ),
            Value::Bool(true)
        ));

        permission.set(ClipboardPermission::Granted);
        realm
            .notify_clipboard_permissions_changed(engine.ctx())
            .unwrap();
        assert!(matches!(
            eval(&mut engine, "permissionStatus.state==='granted' && changes===0"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "permissionStatus.state==='granted' && changes===1"),
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
        eval(&mut engine, "var queryOrder=[]; navigator.permissions.query({name:'clipboard-read'}).then(permissionStatus=>{queryOrder.push(permissionStatus instanceof PermissionStatus && permissionStatus.state==='granted' ? 'query' : 'bad');navigator.permissions.query({name:'clipboard-write'}).then(()=>queryOrder.push('nested'))});Promise.resolve().then(()=>queryOrder.push('microtask'))");
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
        eval(&mut engine, "var firstStatus,secondStatus,permissionOrder=[]; navigator.permissions.query({name:'clipboard-read'}).then(permissionStatus=>{firstStatus=permissionStatus;permissionStatus.onchange=event=>{permissionOrder.push(event.isTrusted && !event.bubbles && !event.cancelable && event.target===permissionStatus ? 'first' : 'bad');Promise.resolve().then(()=>permissionOrder.push('checkpoint'));throw new Error('listener failure')}}); navigator.permissions.query({name:'clipboard-write'}).then(permissionStatus=>{secondStatus=permissionStatus;permissionStatus.addEventListener('change',event=>permissionOrder.push(event.isTrusted && event.currentTarget===permissionStatus ? 'second' : 'bad'))})");
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
            "var permissionStatus, changes=0; navigator.permissions.query({name:'clipboard-read'}).then(value=>{permissionStatus=value;permissionStatus.onchange=()=>changes++})",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(&mut engine, "permissionStatus.state==='granted'"),
            Value::Bool(true)
        ));

        realm
            .browser_services
            .last_user_activation
            .set(Some(Instant::now() - Duration::from_secs(6)));
        assert_eq!(realm.browser_services_delay_ms(), Some(0));
        realm.pump_browser_services(engine.ctx()).unwrap();
        assert!(matches!(
            eval(&mut engine, "permissionStatus.state==='prompt' && changes===0"),
            Value::Bool(true)
        ));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "permissionStatus.state==='prompt' && changes===1 && !navigator.userActivation.isActive && navigator.userActivation.hasBeenActive"
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

    #[test]
    fn navigator_reports_a_chrome_like_profile_and_a_frozen_language_list() {
        let (mut engine, _realm) = install_engine();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                const n = navigator;
                if (!n.userAgent.startsWith('Mozilla/5.0 (X11; Linux aarch64)') || !n.userAgent.includes('Chrome/') || !n.userAgent.endsWith(' Lumen')) return false;
                if (n.appVersion !== n.userAgent.slice('Mozilla/'.length)) return false;
                if (n.platform !== 'Linux aarch64' || n.vendor !== 'Google Inc.' || n.appName !== 'Netscape' || n.product !== 'Gecko' || n.appCodeName !== 'Mozilla') return false;
                if (n.language !== 'en-US' || n.languages.join() !== 'en-US,en' || n.languages !== n.languages || !Object.isFrozen(n.languages)) return false;
                if (n.hardwareConcurrency < 1 || n.maxTouchPoints !== 0 || n.cookieEnabled !== true || n.onLine !== true || n.webdriver !== false) return false;
                if (n.javaEnabled() !== false || n.doNotTrack !== null || n.productSub !== '20030107') return false;
                return Object.getPrototypeOf(n) === Navigator.prototype && 'userAgent' in Navigator.prototype;
            })()"#),
            Value::Bool(true)
        ));
    }

    #[test]
    fn navigator_profile_is_configurable_by_the_embedder() {
        let mut engine = Engine::new();
        set_navigator_profile(engine.ctx(), NavigatorProfile {
            user_agent: "Mozilla/5.0 (Test) Probe".into(),
            platform: "TestOS".into(),
            language: "nl-NL".into(),
            languages: vec!["nl-NL".into(), "nl".into()],
            hardware_concurrency: 8,
            max_touch_points: 5,
            online: false,
            ..NavigatorProfile::default()
        });
        let realm = super::super::install(engine.ctx(), "<main></main>", 32).unwrap();
        install(engine.ctx(), &realm).unwrap();
        assert!(matches!(
            eval(&mut engine, "navigator.userAgent === 'Mozilla/5.0 (Test) Probe' && navigator.appVersion === '5.0 (Test) Probe' && navigator.platform === 'TestOS' && navigator.language === 'nl-NL' && navigator.languages.join() === 'nl-NL,nl' && navigator.hardwareConcurrency === 8 && navigator.maxTouchPoints === 5 && navigator.onLine === false"),
            Value::Bool(true)
        ));
    }

    const BEACON_FETCH: &str = "globalThis.calls = []; globalThis.fetch = (url, init) => { calls.push([url, init.method, init.keepalive, init.body, init.credentials, init.mode]); return Promise.reject(new Error('offline')); }; 0";

    fn beacon_engine() -> (Engine, Rc<DomRealm>) {
        let mut engine = Engine::new();
        eval(&mut engine, BEACON_FETCH);
        let realm = super::super::install(engine.ctx(), "<main></main>", 32).unwrap();
        install(engine.ctx(), &realm).unwrap();
        realm.set_document_url("https://beacon.test/page");
        (engine, realm)
    }

    #[test]
    fn send_beacon_posts_through_fetch_and_validates_the_url() {
        let (mut engine, _realm) = beacon_engine();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                if (navigator.sendBeacon('/log', 'payload') !== true) return false;
                if (calls.length !== 1 || calls[0].join() !== '/log,POST,true,payload,include,no-cors') return false;
                if (navigator.sendBeacon('https://other.test/x') !== true || calls.length !== 2 || calls[1][3] !== undefined) return false;
                for (const bad of ['ftp://x.test/', 'javascript:1', 'http://']) {
                    try { navigator.sendBeacon(bad, 'x'); return false; } catch (e) { if (!(e instanceof TypeError)) return false; }
                }
                try { navigator.sendBeacon(); return false; } catch (e) { return e instanceof TypeError && calls.length === 2; }
            })()"#),
            Value::Bool(true)
        ));
        while engine.run_one_job() {}
    }

    #[test]
    fn send_beacon_ignores_a_fetch_replaced_by_the_page() {
        let (mut engine, _realm) = beacon_engine();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                let hijacked = 0;
                globalThis.fetch = () => { hijacked++; return Promise.resolve(); };
                const sent = navigator.sendBeacon('/log', 'x');
                delete globalThis.fetch;
                return sent === true && hijacked === 0 && calls.length === 1 && navigator.sendBeacon('/again', 'y') === true && calls.length === 2;
            })()"#),
            Value::Bool(true)
        ));
        while engine.run_one_job() {}
    }

    #[test]
    fn send_beacon_uses_cors_only_for_unsafelisted_content_types() {
        let (mut engine, _realm) = beacon_engine();
        engine.define_lazy_globals::<lumen_host::blob::bindings::Module>().ok().unwrap();
        engine.define_lazy_globals::<lumen_host::url::bindings::Module>().ok().unwrap();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                navigator.sendBeacon('/a', new Blob(['{}'], { type: 'application/json' }));
                navigator.sendBeacon('/b', new Blob(['x'], { type: 'text/plain' }));
                navigator.sendBeacon('/c', new URLSearchParams({ k: 'v' }));
                navigator.sendBeacon('/d', new Uint8Array([1, 2, 3]));
                navigator.sendBeacon('/e', new Blob(['x']));
                return calls.map(call => call[5]).join() === 'cors,no-cors,no-cors,no-cors,no-cors';
            })()"#),
            Value::Bool(true)
        ));
        while engine.run_one_job() {}
    }

    #[test]
    fn send_beacon_enforces_the_keepalive_quota_until_requests_settle() {
        let (mut engine, _realm) = beacon_engine();
        assert!(matches!(
            eval(&mut engine, r#"(() => {
                const big = new Uint8Array(40 * 1024);
                if (navigator.sendBeacon('/one', big) !== true) return false;
                if (navigator.sendBeacon('/two', big) !== false || calls.length !== 1) return false;
                if (navigator.sendBeacon('/huge', new Uint8Array(65 * 1024)) !== false) return false;
                return navigator.sendBeacon('/small', 'tiny') === true && calls.length === 2;
            })()"#),
            Value::Bool(true)
        ));
        while engine.run_one_job() {}
        assert!(matches!(
            eval(&mut engine, "navigator.sendBeacon('/after', new Uint8Array(60 * 1024)) === true && calls.length === 3"),
            Value::Bool(true)
        ));
    }
}
