//! Host-backed Notification API and lifecycle delivery.
//!
//! Banner creation, permission prompts, clicks, and closes come from the native host. This
//! adapter never turns a missing host callback into a successful no-op.
use super::*;
use lumen::embed::{Deferred, JsFunction, JsObject};
use lumen_bind::This;
use std::{collections::HashMap, rc::Weak};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationPermission {
    Default,
    Granted,
    Denied,
}

impl Default for NotificationPermission {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationPayload {
    pub id: u64,
    pub origin: String,
    pub title: String,
    pub body: String,
    pub tag: String,
    pub silent: bool,
    pub require_interaction: bool,
}

#[derive(Clone)]
pub struct NotificationHost {
    /// Read the host's current user decision. The host must call
    /// `notify_notification_permission_changed` after external changes.
    pub permission: Rc<dyn Fn(&str) -> NotificationPermission>,
    /// Admit and present the host's permission UI. Completion arrives separately through the
    /// `NotificationPermissionRequest` token.
    pub request_permission: Rc<dyn Fn(NotificationPermissionRequest) -> Result<(), String>>,
    /// Show or replace a real native banner. Success means host state accepted the payload.
    pub show: Rc<dyn Fn(NotificationPayload) -> Result<(), String>>,
    /// Close a previously accepted banner. Success means host state accepted the close request.
    pub close: Rc<dyn Fn(u64) -> Result<(), String>>,
}

#[derive(Clone)]
pub struct NotificationPermissionRequest {
    id: u64,
    origin: String,
    user_activation: bool,
    realm: Weak<DomRealm>,
}

impl NotificationPermissionRequest {
    pub fn id(&self) -> u64 {
        self.id
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn user_activation(&self) -> bool {
        self.user_activation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationHostEvent {
    Show,
    Click,
    Close,
    Error,
}

#[derive(Default)]
pub(super) struct RealmNotifications {
    host: RefCell<Option<NotificationHost>>,
    permission: Cell<NotificationPermission>,
    permission_origin: RefCell<Option<String>>,
    next_id: Cell<u64>,
    notifications: RefCell<HashMap<u64, Rc<NotificationData>>>,
    tags: RefCell<HashMap<String, u64>>,
}

#[derive(Clone)]
struct NotificationOptions {
    body: String,
    tag: String,
    lang: String,
    dir: String,
    icon: String,
    silent: bool,
    require_interaction: bool,
    timestamp: f64,
    data: Value,
}

struct NotificationData {
    id: u64,
    realm: Weak<DomRealm>,
    payload: NotificationPayload,
    lang: String,
    dir: String,
    icon: String,
    timestamp: f64,
    data: Value,
    closed: Cell<bool>,
    closing: Cell<bool>,
    target: DomEventTarget,
    wrapper: RefCell<Option<WeakValue>>,
}

struct PendingPermission {
    promise: Value,
    deferred: Option<Deferred>,
    callbacks: Vec<JsFunction>,
}

#[derive(Default)]
struct NotificationHub {
    realm: Weak<DomRealm>,
    next_permission_request: u64,
    permission_constructor: Option<WeakValue>,
    pending: HashMap<u64, PendingPermission>,
}

impl DomRealm {
    pub fn set_notification_host(self: &Rc<Self>, host: NotificationHost) {
        let origin = self.notification_origin();
        let permission = (host.permission)(&origin);
        *self
            .browser_services
            .notifications
            .permission_origin
            .borrow_mut() = Some(origin);
        self.browser_services
            .notifications
            .permission
            .set(permission);
        *self.browser_services.notifications.host.borrow_mut() = Some(host);
    }

    /// Stable canonical security key for the active document's notification permission.
    /// Tuple origins use WHATWG URL serialization; opaque origins receive a realm-unique key.
    pub fn notification_origin(&self) -> String {
        let Some(input) = self.document_url() else {
            return format!("opaque:realm:{:p}", self);
        };
        let Some(url) = lumen_common::url::parse_url(&input, None) else {
            return format!("opaque:realm:{:p}", self);
        };
        let tuple_scheme = matches!(url.scheme.as_str(), "http" | "https" | "ws" | "wss" | "ftp");
        let trusted_embedder = url.scheme == "bitnest"
            && url.hostname() == "shell"
            && url.username.is_empty()
            && url.password.is_empty()
            && url.port.is_none()
            && !url.opaque;
        if (tuple_scheme || trusted_embedder) && !url.hostname().is_empty() {
            let mut origin = format!("{}://{}", url.scheme, url.hostname());
            if let Some(port) = url.port {
                origin.push(':');
                origin.push_str(&port.to_string());
            }
            origin
        } else {
            format!("opaque:realm:{:p}", self)
        }
    }

    pub fn clear_notification_host(&self) {
        self.browser_services.notifications.host.borrow_mut().take();
        self.browser_services
            .notifications
            .permission
            .set(NotificationPermission::Default);
    }

    pub fn notification_permission(&self) -> NotificationPermission {
        let origin = self.notification_origin();
        let permission = self
            .browser_services
            .notifications
            .host
            .borrow()
            .as_ref()
            .map_or(NotificationPermission::Default, |host| {
                (host.permission)(&origin)
            });
        self.browser_services
            .notifications
            .permission
            .set(permission);
        permission
    }

    /// Re-read permission after a native host change and dispatch PermissionStatus changes.
    pub fn notify_notification_permission_changed(self: &Rc<Self>, ctx: &mut Ctx) -> OpResult<()> {
        let permission = self
            .browser_services
            .notifications
            .host
            .borrow()
            .as_ref()
            .map(|host| (host.permission)(&self.notification_origin()))
            .unwrap_or(NotificationPermission::Default);
        apply_permission(ctx, self, permission)
    }

    /// Complete a host permission prompt. The token is realm-scoped and can settle only once.
    pub fn complete_notification_permission(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        request: &NotificationPermissionRequest,
        permission: NotificationPermission,
    ) -> OpResult<()> {
        let Some(request_realm) = request.realm.upgrade() else {
            return Err(OpError::new(
                "InvalidStateError",
                "notification permission request realm was destroyed",
            ));
        };
        if !Rc::ptr_eq(self, &request_realm) {
            return Err(OpError::new(
                "WrongDocumentError",
                "notification permission request belongs to another realm",
            ));
        }
        let hub = notification_hub(ctx)?;
        let pending = hub
            .borrow_mut()
            .pending
            .remove(&request.id)
            .ok_or_else(|| {
                OpError::new(
                    "InvalidStateError",
                    "notification permission request is no longer pending",
                )
            })?;
        apply_permission(ctx, self, permission)?;
        if let Some(deferred) = pending.deferred {
            deferred.resolve(ctx, permission_name(permission));
        }
        for callback in pending.callbacks {
            let name = permission_name(permission).to_owned();
            scheduling::queue_task(ctx, move |ctx| {
                callback
                    .call(ctx, Value::Undefined, &[Value::from_string(name)])
                    .map(|_| ())
            })?;
        }
        Ok(())
    }

    /// Queue a real native show/click/close/error outcome as a user-agent task.
    pub fn notify_notification_event(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        id: u64,
        event: NotificationHostEvent,
    ) -> OpResult<()> {
        let weak = Rc::downgrade(self);
        scheduling::queue_task(ctx, move |ctx| {
            let Some(realm) = weak.upgrade() else {
                return Ok(());
            };
            let Some(data) = realm
                .browser_services
                .notifications
                .notifications
                .borrow()
                .get(&id)
                .cloned()
            else {
                return Ok(());
            };
            if data.closed.get() {
                return Ok(());
            }
            let event_name = match event {
                NotificationHostEvent::Show => "show",
                NotificationHostEvent::Click => "click",
                NotificationHostEvent::Close => "close",
                NotificationHostEvent::Error => "error",
            };
            if matches!(
                event,
                NotificationHostEvent::Close | NotificationHostEvent::Error
            ) {
                data.closed.set(true);
                data.closing.set(false);
                realm
                    .browser_services
                    .notifications
                    .notifications
                    .borrow_mut()
                    .remove(&id);
                if !data.payload.tag.is_empty()
                    && realm
                        .browser_services
                        .notifications
                        .tags
                        .borrow()
                        .get(&data.payload.tag)
                        == Some(&id)
                {
                    realm
                        .browser_services
                        .notifications
                        .tags
                        .borrow_mut()
                        .remove(&data.payload.tag);
                }
            }
            dispatch_notification_event(ctx, &data, event_name)
        })
    }
}

#[lumen_bind::class(name = "Notification", extends = DomEventTarget, hint(js(webidl)))]
pub struct DomNotification {
    base: DomEventTarget,
    data: Rc<NotificationData>,
}

#[lumen_bind::methods]
impl DomNotification {
    #[constructor]
    fn new(
        ctx: &mut Ctx,
        this: This<Value>,
        title: &str,
        options: Option<Value>,
    ) -> OpResult<Self> {
        let hub = notification_hub(ctx)?;
        let realm =
            hub.borrow().realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "notification realm was destroyed")
            })?;
        let host = realm
            .browser_services
            .notifications
            .host
            .borrow()
            .clone()
            .ok_or_else(|| {
                OpError::new(
                    "NotSupportedError",
                    "the host has not supplied notifications",
                )
            })?;
        let permission = realm.notification_permission();
        if permission != NotificationPermission::Granted {
            return Err(OpError::new(
                "NotAllowedError",
                "notifications are not permitted by the host",
            ));
        }
        let options = parse_options(ctx, options)?;
        let id = {
            let state = &realm.browser_services.notifications.next_id;
            state.set(state.get().wrapping_add(1).max(1));
            state.get()
        };
        let origin = realm.notification_origin();
        let payload = NotificationPayload {
            id,
            origin,
            title: title.to_owned(),
            body: options.body.clone(),
            tag: options.tag.clone(),
            silent: options.silent,
            require_interaction: options.require_interaction,
        };
        let data = Rc::new(NotificationData {
            id,
            realm: Rc::downgrade(&realm),
            payload,
            lang: options.lang,
            dir: options.dir,
            icon: options.icon,
            timestamp: options.timestamp,
            data: options.data,
            closed: Cell::new(false),
            closing: Cell::new(false),
            target: DomEventTarget::independent(&realm),
            wrapper: RefCell::new(ctx.weak_value(&this.0)),
        });
        (host.show)(data.payload.clone()).map_err(|message| {
            OpError::new(
                "UnknownError",
                format!("native notification show failed: {message}"),
            )
        })?;
        realm
            .browser_services
            .notifications
            .notifications
            .borrow_mut()
            .insert(id, data.clone());
        if !data.payload.tag.is_empty() {
            realm
                .browser_services
                .notifications
                .tags
                .borrow_mut()
                .insert(data.payload.tag.clone(), id);
        }
        Ok(Self {
            base: data.target.clone(),
            data,
        })
    }

    #[classmethod]
    fn request_permission(
        ctx: &mut Ctx,
        _class: This<Value>,
        callback: Option<JsFunction>,
    ) -> OpResult<Value> {
        request_permission(ctx, callback)
    }

    #[getter]
    fn title(&self) -> String {
        self.data.payload.title.clone()
    }
    #[getter]
    fn body(&self) -> String {
        self.data.payload.body.clone()
    }
    #[getter]
    fn tag(&self) -> String {
        self.data.payload.tag.clone()
    }
    #[getter]
    fn lang(&self) -> String {
        self.data.lang.clone()
    }
    #[getter]
    fn dir(&self) -> String {
        self.data.dir.clone()
    }
    #[getter]
    fn icon(&self) -> String {
        self.data.icon.clone()
    }
    #[getter]
    fn timestamp(&self) -> f64 {
        self.data.timestamp
    }
    #[getter]
    fn silent(&self) -> bool {
        self.data.payload.silent
    }
    #[getter]
    fn require_interaction(&self) -> bool {
        self.data.payload.require_interaction
    }
    #[getter]
    fn data(&self) -> Value {
        self.data.data.clone()
    }
    #[getter]
    fn closed(&self) -> bool {
        self.data.closed.get()
    }

    fn close(&self) -> OpResult<()> {
        if self.data.closed.get() || self.data.closing.get() {
            return Ok(());
        }
        let realm =
            self.data.realm.upgrade().ok_or_else(|| {
                OpError::new("InvalidStateError", "notification realm was destroyed")
            })?;
        let callback = realm
            .browser_services
            .notifications
            .host
            .borrow()
            .as_ref()
            .map(|host| host.close.clone())
            .ok_or_else(|| {
                OpError::new(
                    "NotSupportedError",
                    "the host has not supplied notification close",
                )
            })?;
        callback(self.data.id).map_err(|message| {
            OpError::new(
                "UnknownError",
                format!("native notification close failed: {message}"),
            )
        })?;
        self.data.closing.set(true);
        Ok(())
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

    fn remove_event_listener(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        kind: &str,
        callback: Value,
        options: Option<Value>,
    ) -> OpResult<()> {
        crate::events::remove_event_listener(ctx, this, kind, callback, options)
    }

    #[getter]
    fn onclick(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "click")
    }
    #[setter]
    fn set_onclick(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "click", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
    #[getter]
    fn onshow(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "show")
    }
    #[setter]
    fn set_onshow(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "show", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
    #[getter]
    fn onclose(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "close")
    }
    #[setter]
    fn set_onclose(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "close", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
    #[getter]
    fn onerror(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "error")
    }
    #[setter]
    fn set_onerror(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "error", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

fn parse_options(ctx: &mut Ctx, options: Option<Value>) -> OpResult<NotificationOptions> {
    let options = match options {
        None | Some(Value::Undefined | Value::Null) => None,
        Some(value @ Value::Obj(_)) => Some(value),
        Some(_) => {
            return Err(OpError::new(
                "TypeError",
                "notification options must be a dictionary",
            ));
        }
    };
    let string = |ctx: &mut Ctx, name: &str| -> OpResult<String> {
        let Some(options) = options.as_ref() else {
            return Ok(String::new());
        };
        let value = ctx.member_get(options, name).map_err(OpError::thrown)?;
        if matches!(value, Value::Undefined) {
            Ok(String::new())
        } else {
            Ok(ctx
                .coerce_string(&value)
                .map_err(OpError::thrown)?
                .to_string())
        }
    };
    let boolean = |ctx: &mut Ctx, name: &str| -> OpResult<bool> {
        let Some(options) = options.as_ref() else {
            return Ok(false);
        };
        let value = ctx.member_get(options, name).map_err(OpError::thrown)?;
        Ok(ctx.to_boolean(&value))
    };
    let body = string(ctx, "body")?;
    let tag = string(ctx, "tag")?;
    let lang = string(ctx, "lang")?;
    let dir = match string(ctx, "dir")?.as_str() {
        "" => "auto".to_owned(),
        value @ ("auto" | "ltr" | "rtl") => value.to_owned(),
        _ => {
            return Err(OpError::new(
                "TypeError",
                "notification dir must be auto, ltr, or rtl",
            ));
        }
    };
    let icon = string(ctx, "icon")?;
    let silent = boolean(ctx, "silent")?;
    let require_interaction = boolean(ctx, "requireInteraction")?;
    let timestamp = if let Some(options) = options.as_ref() {
        let value = ctx
            .member_get(options, "timestamp")
            .map_err(OpError::thrown)?;
        if matches!(value, Value::Undefined) {
            lumen_host::perf::now_ms()
        } else {
            let value = ctx.coerce_number(&value).map_err(OpError::thrown)?;
            if !value.is_finite() {
                return Err(OpError::new(
                    "TypeError",
                    "notification timestamp must be finite",
                ));
            }
            value
        }
    } else {
        lumen_host::perf::now_ms()
    };
    let data = if let Some(options) = options.as_ref() {
        ctx.member_get(options, "data").map_err(OpError::thrown)?
    } else {
        Value::Null
    };
    Ok(NotificationOptions {
        body,
        tag,
        lang,
        dir,
        icon,
        silent,
        require_interaction,
        timestamp,
        data,
    })
}

fn notification_hub(ctx: &mut Ctx) -> OpResult<Rc<RefCell<NotificationHub>>> {
    ctx.op_state()
        .get::<Rc<RefCell<NotificationHub>>>()
        .cloned()
        .ok_or_else(|| OpError::new("InvalidStateError", "Notifications are not installed"))
}

fn request_permission(ctx: &mut Ctx, callback: Option<JsFunction>) -> OpResult<Value> {
    let hub = notification_hub(ctx)?;
    let realm = hub
        .borrow()
        .realm
        .upgrade()
        .ok_or_else(|| OpError::new("InvalidStateError", "notification realm was destroyed"))?;
    let permission = realm.notification_permission();
    if permission != NotificationPermission::Default {
        let deferred = Deferred::new(ctx);
        let promise = deferred.promise();
        deferred.resolve(ctx, permission_name(permission));
        if let Some(callback) = callback {
            queue_permission_callback(ctx, callback, permission)?;
        }
        return Ok(promise);
    }
    if let Some(pending) = hub.borrow_mut().pending.values_mut().next() {
        if let Some(callback) = callback {
            pending.callbacks.push(callback);
        }
        return Ok(pending.promise.clone());
    }
    if !realm.has_transient_user_activation() {
        return Ok(rejected_promise(
            ctx,
            OpError::new(
                "NotAllowedError",
                "requestPermission requires trusted user activation",
            ),
        ));
    }
    let Some(host) = realm.browser_services.notifications.host.borrow().clone() else {
        return Ok(rejected_promise(
            ctx,
            OpError::new(
                "NotSupportedError",
                "the host has not supplied notification permission UI",
            ),
        ));
    };
    if !realm.consume_user_activation() {
        return Ok(rejected_promise(
            ctx,
            OpError::new(
                "NotAllowedError",
                "requestPermission requires trusted user activation",
            ),
        ));
    }
    let request_id = {
        let mut state = hub.borrow_mut();
        state.next_permission_request = state.next_permission_request.wrapping_add(1).max(1);
        state.next_permission_request
    };
    let request = NotificationPermissionRequest {
        id: request_id,
        origin: realm.notification_origin(),
        user_activation: true,
        realm: Rc::downgrade(&realm),
    };
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    let pending = PendingPermission {
        promise: promise.clone(),
        deferred: Some(deferred),
        callbacks: callback.into_iter().collect(),
    };
    hub.borrow_mut().pending.insert(request_id, pending);
    if let Err(message) = (host.request_permission)(request) {
        if let Some(mut pending) = hub.borrow_mut().pending.remove(&request_id) {
            if let Some(deferred) = pending.deferred.take() {
                deferred.reject(
                    ctx,
                    OpError::new(
                        "UnknownError",
                        format!("native notification permission request failed: {message}"),
                    ),
                );
            }
        }
    }
    Ok(promise)
}

fn rejected_promise(ctx: &mut Ctx, error: OpError) -> Value {
    let deferred = Deferred::new(ctx);
    let promise = deferred.promise();
    deferred.reject(ctx, error);
    promise
}

fn queue_permission_callback(
    ctx: &mut Ctx,
    callback: JsFunction,
    permission: NotificationPermission,
) -> OpResult<()> {
    let name = permission_name(permission).to_owned();
    scheduling::queue_task(ctx, move |ctx| {
        callback
            .call(ctx, Value::Undefined, &[Value::from_string(name)])
            .map(|_| ())
    })
}

fn permission_name(permission: NotificationPermission) -> &'static str {
    match permission {
        NotificationPermission::Default => "default",
        NotificationPermission::Granted => "granted",
        NotificationPermission::Denied => "denied",
    }
}

fn apply_permission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    permission: NotificationPermission,
) -> OpResult<()> {
    realm
        .browser_services
        .notifications
        .permission
        .set(permission);
    *realm
        .browser_services
        .notifications
        .permission_origin
        .borrow_mut() = Some(realm.notification_origin());
    if let Some(hub) = ctx
        .op_state()
        .get::<Rc<RefCell<NotificationHub>>>()
        .cloned()
    {
        let constructor = hub
            .borrow()
            .permission_constructor
            .as_ref()
            .and_then(WeakValue::upgrade);
        if let Some(constructor) = constructor {
            ctx.set_member(
                &constructor,
                "permission",
                Value::from_string(permission_name(permission).to_owned()),
            )
            .map_err(|_| OpError::new("TypeError", "Notification.permission update failed"))?;
        }
    }
    realm
        .browser_services
        .refresh_notification_permission_statuses(ctx, realm)
}

pub(crate) fn pump_permission_origin(ctx: &mut Ctx, realm: &DomRealm) -> OpResult<()> {
    let origin = realm.notification_origin();
    let changed = realm
        .browser_services
        .notifications
        .permission_origin
        .borrow()
        .as_deref()
        != Some(origin.as_str());
    let host = realm.browser_services.notifications.host.borrow().clone();
    let permission = host
        .as_ref()
        .map_or(NotificationPermission::Default, |host| {
            (host.permission)(&origin)
        });
    let old_permission = realm
        .browser_services
        .notifications
        .permission
        .replace(permission);
    *realm
        .browser_services
        .notifications
        .permission_origin
        .borrow_mut() = Some(origin);
    if changed || old_permission != permission {
        if let Some(hub) = ctx
            .op_state()
            .get::<Rc<RefCell<NotificationHub>>>()
            .cloned()
        {
            if let Some(constructor) = hub
                .borrow()
                .permission_constructor
                .as_ref()
                .and_then(WeakValue::upgrade)
            {
                ctx.set_member(
                    &constructor,
                    "permission",
                    Value::from_string(permission_name(permission).to_owned()),
                )
                .map_err(|_| OpError::new("TypeError", "Notification.permission update failed"))?;
            }
        }
        realm
            .browser_services
            .refresh_notification_permission_statuses(ctx, realm)?;
    }
    Ok(())
}

fn dispatch_notification_event(ctx: &mut Ctx, data: &NotificationData, kind: &str) -> OpResult<()> {
    let Some(wrapper) = data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) else {
        return Ok(());
    };
    let options = Value::Obj(ctx.new_object());
    ctx.set_member(&options, "bubbles", Value::Bool(false))
        .and_then(|_| ctx.set_member(&options, "cancelable", Value::Bool(false)))
        .map_err(|_| OpError::new("TypeError", "notification event setup failed"))?;
    let event = DomEvent::new(ctx, kind, Some(options))?;
    let event = JsObject::from_value(ctx.new_instance(event))
        .ok_or_else(|| OpError::new("TypeError", "notification event is invalid"))?;
    crate::events::dispatch_event(ctx, This(wrapper), event).map(|_| ())
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let hub = Rc::new(RefCell::new(NotificationHub {
        realm: Rc::downgrade(realm),
        ..NotificationHub::default()
    }));
    ctx.op_state().put(hub.clone());
    let constructor = ctx.class_constructor::<DomNotification>();
    hub.borrow_mut().permission_constructor = ctx.weak_value(&constructor);
    let permission = permission_name(realm.notification_permission());
    ctx.set_member(
        &constructor,
        "permission",
        Value::from_string(permission.to_owned()),
    )
    .map_err(|_| OpError::new("Error", "Notification.permission installation failed"))?;
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "Notification", constructor)
        .map_err(|_| OpError::new("Error", "Notification installation failed"))?;
    Ok(())
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
        (engine, realm)
    }

    #[test]
    fn notification_origins_are_canonical_and_opaque_realms_do_not_collide() {
        let (_, first) = install_engine();
        let (_, second) = install_engine();
        first.set_document_url("https://ExAmPlE.com:443/a");
        second.set_document_url("https://example.com/b");
        assert_eq!(first.notification_origin(), "https://example.com");
        assert_eq!(first.notification_origin(), second.notification_origin());
        first.set_document_url("bitnest://shell/");
        assert_eq!(first.notification_origin(), "bitnest://shell");
        first.set_document_url("data:text/html,one");
        second.set_document_url("data:text/html,two");
        assert!(first.notification_origin().starts_with("opaque:realm:"));
        assert_ne!(first.notification_origin(), second.notification_origin());
    }

    #[test]
    fn permission_prompt_is_origin_scoped_and_callbacks_run_as_tasks() {
        let (mut engine, realm) = install_engine();
        realm.set_document_url("https://notify.example/path");
        let permissions = Rc::new(RefCell::new(HashMap::new()));
        let prompt_requests = Rc::new(RefCell::new(Vec::new()));
        let permission_state = permissions.clone();
        let prompts = prompt_requests.clone();
        realm.set_notification_host(NotificationHost {
            permission: Rc::new(move |origin| {
                permission_state
                    .borrow()
                    .get(origin)
                    .copied()
                    .unwrap_or(NotificationPermission::Default)
            }),
            request_permission: Rc::new(move |request| {
                prompts.borrow_mut().push(request);
                Ok(())
            }),
            show: Rc::new(|_| Ok(())),
            close: Rc::new(|_| Ok(())),
        });
        eval(
            &mut engine,
            "var notificationStatus, permissionChanges=0; navigator.permissions.query({name:'notifications'}).then(status=>{notificationStatus=status;status.onchange=()=>permissionChanges++})",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(super::super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "notificationStatus instanceof PermissionStatus && notificationStatus.name==='notifications' && notificationStatus.state==='prompt'"
            ),
            Value::Bool(true)
        ));
        realm.mark_user_activation();
        assert!(matches!(
            eval(
                &mut engine,
                "var permissionResult='pending', legacyResult=''; var permissionPromise=Notification.requestPermission(value=>legacyResult=value); Notification.requestPermission().then(value=>permissionResult=value); permissionPromise===Notification.requestPermission()"
            ),
            Value::Bool(true)
        ));
        assert_eq!(prompt_requests.borrow().len(), 1);
        let request = prompt_requests.borrow()[0].clone();
        assert_eq!(request.origin(), "https://notify.example");
        assert!(request.user_activation());
        permissions
            .borrow_mut()
            .insert(request.origin().to_owned(), NotificationPermission::Granted);
        realm
            .complete_notification_permission(
                engine.ctx(),
                &request,
                NotificationPermission::Granted,
            )
            .unwrap();
        assert!(matches!(
            eval(
                &mut engine,
                "permissionResult==='pending' && legacyResult==='' && permissionChanges===0"
            ),
            Value::Bool(true)
        ));
        assert!(super::super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "permissionResult==='granted' && legacyResult==='granted' && Notification.permission==='granted' && notificationStatus.state==='granted' && permissionChanges===1"
            ),
            Value::Bool(true)
        ));

        realm.set_document_url("https://other.example/");
        realm.pump_browser_services(engine.ctx()).unwrap();
        assert!(super::super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "Notification.permission==='default' && notificationStatus.state==='prompt' && permissionChanges===2"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn permission_denial_and_missing_host_never_create_a_banner() {
        let (mut engine, realm) = install_engine();
        realm.set_document_url("https://denied.example/");
        eval(
            &mut engine,
            "var missingPermission=''; Notification.requestPermission().catch(error=>missingPermission=error.name)",
        );
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(
            eval(&mut engine, "missingPermission==='NotAllowedError'"),
            Value::Bool(true)
        ));

        let prompts = Rc::new(RefCell::new(Vec::new()));
        let prompt_log = prompts.clone();
        realm.set_notification_host(NotificationHost {
            permission: Rc::new(|_| NotificationPermission::Default),
            request_permission: Rc::new(move |request| {
                prompt_log.borrow_mut().push(request);
                Ok(())
            }),
            show: Rc::new(|_| panic!("denied permission cannot reach show")),
            close: Rc::new(|_| Ok(())),
        });
        realm.mark_user_activation();
        eval(
            &mut engine,
            "var denial='pending'; Notification.requestPermission().then(value=>denial=value)",
        );
        let request = prompts.borrow()[0].clone();
        realm
            .complete_notification_permission(
                engine.ctx(),
                &request,
                NotificationPermission::Denied,
            )
            .unwrap();
        super::super::scheduling::run_tasks(&mut engine, 8);
        assert!(matches!(
            eval(
                &mut engine,
                "denial==='denied' && Notification.permission==='denied' && (()=>{try{new Notification('blocked');return false}catch(error){return error.name==='NotAllowedError'}})()"
            ),
            Value::Bool(true)
        ));
    }

    #[test]
    fn native_notification_events_and_close_follow_host_acceptance() {
        let (mut engine, realm) = install_engine();
        realm.set_document_url("bitnest://shell/");
        let shown = Rc::new(RefCell::new(Vec::<NotificationPayload>::new()));
        let closed = Rc::new(RefCell::new(Vec::<u64>::new()));
        let show_log = shown.clone();
        let close_log = closed.clone();
        realm.set_notification_host(NotificationHost {
            permission: Rc::new(|origin| {
                if origin == "bitnest://shell" {
                    NotificationPermission::Granted
                } else {
                    NotificationPermission::Denied
                }
            }),
            request_permission: Rc::new(|_| Err("permission prompt should not be needed".into())),
            show: Rc::new(move |payload| {
                show_log.borrow_mut().push(payload);
                Ok(())
            }),
            close: Rc::new(move |id| {
                close_log.borrow_mut().push(id);
                Ok(())
            }),
        });
        assert!(matches!(
            eval(
                &mut engine,
                "var notificationEvents=[]; var notice=new Notification('Build finished',{body:'Ready',tag:'build',requireInteraction:true}); notice.onshow=()=>notificationEvents.push('show'); notice.addEventListener('show',function(event){notificationEvents.push(this===notice && event.target===notice ? 'identity':'bad')}); notice.onclick=()=>notificationEvents.push('click'); notice.onclose=()=>notificationEvents.push('close'); notice instanceof Notification && notice instanceof EventTarget && !notice.closed && notice.body==='Ready' && notice.requireInteraction"
            ),
            Value::Bool(true)
        ));
        let id = shown.borrow()[0].id;
        assert_eq!(shown.borrow()[0].origin, "bitnest://shell");
        realm
            .notify_notification_event(engine.ctx(), id, NotificationHostEvent::Show)
            .unwrap();
        realm
            .notify_notification_event(engine.ctx(), id, NotificationHostEvent::Click)
            .unwrap();
        assert!(matches!(
            eval(&mut engine, "notificationEvents.length===0"),
            Value::Bool(true)
        ));
        assert!(super::super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "notificationEvents.join(',')==='show,identity,click'"
            ),
            Value::Bool(true)
        ));
        eval(&mut engine, "notice.close(); notice.close()");
        assert_eq!(&*closed.borrow(), &[id]);
        assert!(matches!(
            eval(&mut engine, "!notice.closed"),
            Value::Bool(true)
        ));
        realm
            .notify_notification_event(engine.ctx(), id, NotificationHostEvent::Close)
            .unwrap();
        assert!(super::super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            eval(
                &mut engine,
                "notice.closed && notificationEvents.join(',')==='show,identity,click,close'"
            ),
            Value::Bool(true)
        ));
    }
}
