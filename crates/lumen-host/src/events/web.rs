//! The Web-exposed event classes.

#[lumen_bind::module(name = "webEvents")]
pub mod bindings {
    use super::super::abort::{self, SignalState};
    use super::super::*;
    use crate::webidl::usv_string;
    use lumen::embed::JsHost;
    use lumen_bind::{CtorRet, Host, This};

    #[derive(Clone)]
    #[class(name = "Event", hint(js(webidl, invalid_this)))]
    pub struct Event {
        pub(crate) state: Rc<EventState>,
    }

    #[derive(Clone)]
    #[class(name = "EventTarget", hint(js(webidl, invalid_this)))]
    pub struct EventTarget {
        pub(crate) data: Rc<TargetData>,
    }

    #[class(name = "CustomEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct CustomEvent {
        base: Event,
        detail_slot: String,
    }

    #[class(name = "ErrorEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct ErrorEvent {
        base: Event,
        message: String,
        filename: String,
        lineno: u32,
        colno: u32,
        error_slot: String,
    }

    #[class(name = "AbortSignal", extends = EventTarget, hint(js(webidl, invalid_this)))]
    pub struct AbortSignal {
        pub(crate) base: EventTarget,
        pub(crate) state: Rc<SignalState>,
    }

    #[class(name = "AbortController", hint(js(webidl, invalid_this)))]
    pub struct AbortController {
        signal_slot: String,
    }

    #[class(name = "DOMException", hint(js(webidl, error, invalid_this)))]
    pub struct DomException {
        name: String,
        message: String,
    }

    // ---- Event -------------------------------------------------------------------------------

    #[methods]
    impl Event {
        #[constant(name = "NONE")]
        const NONE: u16 = 0;
        #[constant(name = "CAPTURING_PHASE")]
        const CAPTURING_PHASE: u16 = 1;
        #[constant(name = "AT_TARGET")]
        const AT_TARGET: u16 = 2;
        #[constant(name = "BUBBLING_PHASE")]
        const BUBBLING_PHASE: u16 = 3;

        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        pub fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
            Self::new(ctx, kind, options)
        }

        #[getter(name = "type")]
        fn get_type(&self) -> String {
            self.kind.borrow().clone()
        }

        #[getter]
        fn target(&self) -> Value {
            self.target.borrow().clone()
        }

        #[getter(name = "srcElement")]
        fn src_element(&self) -> Value {
            self.target.borrow().clone()
        }

        #[getter]
        fn current_target(&self) -> Value {
            self.current.borrow().clone()
        }

        fn composed_path(&self, ctx: &mut Ctx) -> Value {
            let visible = self.visibility.borrow();
            let path = self
                .path
                .borrow()
                .iter()
                .filter(|(_, closed)| closed.iter().all(|scope| visible.contains(scope)))
                .map(|(value, _)| value.clone())
                .collect();
            ctx.make_array(path)
        }

        #[getter]
        fn event_phase(&self) -> u8 {
            self.phase.get()
        }

        fn stop_propagation(&self) {
            self.stopped.set(true);
        }

        #[getter(name = "cancelBubble")]
        fn cancel_bubble(&self) -> bool {
            self.stopped.get()
        }

        #[setter(name = "cancelBubble", coerce)]
        fn set_cancel_bubble(&self, value: bool) {
            if value {
                self.stopped.set(true);
            }
        }

        fn stop_immediate_propagation(&self) {
            self.stopped.set(true);
            self.immediate.set(true);
        }

        #[getter]
        fn bubbles(&self) -> bool {
            self.bubbles.get()
        }

        #[getter]
        fn cancelable(&self) -> bool {
            self.cancelable.get()
        }

        #[getter(name = "returnValue")]
        fn return_value(&self) -> bool {
            !self.canceled.get()
        }

        #[setter(name = "returnValue", coerce)]
        fn set_return_value(&self, value: bool) {
            if !value {
                self.prevent_default();
            }
        }

        #[method(name = "preventDefault")]
        fn prevent_default_op(&self) {
            self.prevent_default();
        }

        #[getter(name = "defaultPrevented")]
        fn get_default_prevented(&self) -> bool {
            self.canceled.get()
        }

        #[getter]
        fn composed(&self) -> bool {
            self.composed.get()
        }

        #[getter(hint(js(unforgeable)))]
        fn is_trusted(&self) -> bool {
            self.trusted.get()
        }

        #[getter]
        fn time_stamp(&self) -> f64 {
            self.time_stamp
        }

        #[method(name = "initEvent", coerce)]
        fn init_event(
            &self,
            kind: &str,
            #[default(false)] bubbles: bool,
            #[default(false)] cancelable: bool,
        ) {
            let _ = self.initialize_legacy(kind, bubbles, cancelable);
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            let fields = vec![
                ("type", Value::str(self.kind.borrow().as_str())),
                ("defaultPrevented", Value::Bool(self.canceled.get())),
                ("cancelable", Value::Bool(self.cancelable.get())),
                ("timeStamp", Value::Num(self.time_stamp)),
            ];
            inspect_object(ctx, &this.0, fields, &depth, &options, &inspect, true)
        }
    }

    // ---- EventTarget -------------------------------------------------------------------------

    #[methods]
    impl EventTarget {
        #[constructor]
        pub fn new() -> Self {
            Self::from_data(TargetData::new(None))
        }

        #[method(hint(js(
            missing_message = "The \"type\" and \"listener\" arguments must be specified",
            missing_code = "ERR_MISSING_ARGS"
        )))]
        pub fn add_event_listener(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            callback: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            let (data, receiver) = Self::of_receiver(ctx, &this.0)?;
            let kind = ctx.coerce_string(&kind).map_err(OpError::thrown)?;
            let options = ListenerOptions::read(ctx, &options, true)?;
            Self::add_listener(ctx, &receiver, &data, &kind, callback, options)
        }

        #[method(hint(js(
            missing_message = "The \"type\" and \"listener\" arguments must be specified",
            missing_code = "ERR_MISSING_ARGS"
        )))]
        pub fn remove_event_listener(
            ctx: &mut Ctx,
            this: This<Value>,
            kind: Value,
            callback: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<()> {
            let (data, receiver) = Self::of_receiver(ctx, &this.0)?;
            let kind = ctx.coerce_string(&kind).map_err(OpError::thrown)?;
            let capture = ListenerOptions::read(ctx, &options, false)?.capture;
            if !matches!(callback, Value::Obj(_)) {
                return Ok(());
            }
            Self::remove_listener(ctx, &receiver, &data, &kind, &callback, capture)
        }

        #[method(hint(js(
            missing_message = "The \"event\" argument must be specified",
            missing_code = "ERR_MISSING_ARGS"
        )))]
        pub fn dispatch_event(ctx: &mut Ctx, this: This<Value>, event: Value) -> OpResult<bool> {
            Self::dispatch(ctx, &this.0, &event)
        }

        /// Node's `kEvents`: a snapshot `Map` of type to listener callbacks.
        #[getter(hint(js(symbol_for = "lumen.kEvents")))]
        fn events_map(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            let (data, _) = Self::of_receiver(ctx, &this.0)?;
            let entries = data
                .event_names()
                .into_iter()
                .map(|name| {
                    let callbacks = ctx.make_array(data.callbacks(&name));
                    ctx.make_array(vec![Value::str(&*name), callbacks])
                })
                .collect();
            let entries = ctx.make_array(entries);
            let global = ctx.global_object();
            let map = ctx.member_get(&global, "Map").map_err(OpError::thrown)?;
            ctx.construct_value(map, &[entries]).map_err(OpError::thrown)
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            Self::of_receiver(ctx, &this.0)?;
            inspect_object(ctx, &this.0, Vec::new(), &depth, &options, &inspect, true)
        }
    }

    impl lumen::embed::NativeIdentityOwner for EventTarget {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.data.trace_callbacks(visit);
        }
    }

    impl Default for EventTarget {
        fn default() -> Self {
            Self::new()
        }
    }

    // ---- CustomEvent -------------------------------------------------------------------------

    pub struct CustomEventConstructor {
        event: CustomEvent,
        detail: Value,
    }

    impl CtorRet<JsHost, CustomEvent> for CustomEventConstructor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let slot = self.event.detail_slot.clone();
            let instance = <JsHost as Host>::construct(cx, self.event)?;
            <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                ctx.define_native_private_value_slot(&instance, &slot, self.detail)
            })?;
            Ok(instance)
        }
    }

    #[methods]
    impl CustomEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(
            ctx: &mut Ctx,
            kind: &str,
            options: Option<Value>,
        ) -> OpResult<CustomEventConstructor> {
            let base = Event::new(ctx, kind, options.clone())?;
            let detail = match &options {
                Some(options @ Value::Obj(_)) => {
                    ctx.member_get(options, "detail").map_err(OpError::thrown)?
                }
                _ => Value::Undefined,
            };
            let detail = match detail {
                Value::Undefined => Value::Null,
                detail => detail,
            };
            Ok(CustomEventConstructor {
                event: CustomEvent {
                    base,
                    detail_slot: ctx.allocate_native_private_slot_name(),
                },
                detail,
            })
        }

        #[getter]
        fn detail(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.detail_slot)
                .unwrap_or(Value::Null)
        }

        #[method(name = "initCustomEvent", coerce)]
        fn init_custom_event(
            &self,
            kind: &str,
            #[default(false)] bubbles: bool,
            #[default(false)] cancelable: bool,
        ) {
            let _ = self.base.initialize_legacy(kind, bubbles, cancelable);
        }
    }

    impl CustomEvent {
        /// A `CustomEvent` of `kind` carrying `detail` (Node's `emit`).
        pub fn create(ctx: &mut Ctx, kind: &str, detail: Value) -> OpResult<Value> {
            let slot = ctx.allocate_native_private_slot_name();
            let instance = ctx.new_instance(CustomEvent {
                base: Event::from_init(kind, EventInit::default()),
                detail_slot: slot.clone(),
            });
            ctx.define_native_private_value_slot(&instance, &slot, detail)
                .map_err(OpError::thrown)?;
            Ok(instance)
        }
    }

    // ---- ErrorEvent --------------------------------------------------------------------------

    pub struct ErrorEventConstructor {
        event: ErrorEvent,
        error: Value,
    }

    impl CtorRet<JsHost, ErrorEvent> for ErrorEventConstructor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let slot = self.event.error_slot.clone();
            let instance = <JsHost as Host>::construct(cx, self.event)?;
            <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                ctx.define_native_private_value_slot(&instance, &slot, self.error)
            })?;
            Ok(instance)
        }
    }

    fn member(ctx: &mut Ctx, options: &Option<Value>, key: &str) -> OpResult<Value> {
        match options {
            Some(options @ Value::Obj(_)) => {
                ctx.member_get(options, key).map_err(OpError::thrown)
            }
            _ => Ok(Value::Undefined),
        }
    }

    fn unsigned_long(ctx: &mut Ctx, value: &Value) -> OpResult<u32> {
        if matches!(value, Value::Undefined) {
            return Ok(0);
        }
        let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
        if !number.is_finite() {
            return Ok(0);
        }
        Ok(number.trunc().rem_euclid(4294967296.0) as u32)
    }

    #[methods]
    impl ErrorEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(
            ctx: &mut Ctx,
            kind: &str,
            options: Option<Value>,
        ) -> OpResult<ErrorEventConstructor> {
            let base = Event::new(ctx, kind, options.clone())?;
            let colno = member(ctx, &options, "colno")?;
            let colno = unsigned_long(ctx, &colno)?;
            let error = member(ctx, &options, "error")?;
            let filename = match member(ctx, &options, "filename")? {
                Value::Undefined => String::new(),
                value => usv_string(ctx, &value).map_err(OpError::thrown)?,
            };
            let lineno = member(ctx, &options, "lineno")?;
            let lineno = unsigned_long(ctx, &lineno)?;
            let message = match member(ctx, &options, "message")? {
                Value::Undefined => String::new(),
                value => ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string(),
            };
            Ok(ErrorEventConstructor {
                event: ErrorEvent {
                    base,
                    message,
                    filename,
                    lineno,
                    colno,
                    error_slot: ctx.allocate_native_private_slot_name(),
                },
                error,
            })
        }

        #[getter]
        fn message(&self) -> String {
            self.message.clone()
        }

        #[getter]
        fn filename(&self) -> String {
            self.filename.clone()
        }

        #[getter]
        fn lineno(&self) -> u32 {
            self.lineno
        }

        #[getter]
        fn colno(&self) -> u32 {
            self.colno
        }

        #[getter]
        fn error(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.error_slot)
                .unwrap_or(Value::Undefined)
        }
    }

    impl ErrorEvent {
        /// A trusted-to-be `ErrorEvent` the user agent creates (`reportError`, worker errors).
        pub fn create(
            ctx: &mut Ctx,
            kind: &str,
            init: EventInit,
            message: &str,
            filename: &str,
            lineno: u32,
            colno: u32,
            error: Value,
        ) -> OpResult<Value> {
            let slot = ctx.allocate_native_private_slot_name();
            let instance = ctx.new_instance(ErrorEvent {
                base: Event::from_init(kind, init),
                message: message.into(),
                filename: filename.into(),
                lineno,
                colno,
                error_slot: slot.clone(),
            });
            ctx.define_native_private_value_slot(&instance, &slot, error)
                .map_err(OpError::thrown)?;
            Ok(instance)
        }

        /// The five arguments of a global `onerror` handler, read from the native fields so no
        /// author getter runs: `(message, filename, lineno, colno, error)`. `None` when `event`
        /// is not an `ErrorEvent`.
        pub fn handler_arguments(ctx: &mut Ctx, event: &Value) -> Option<Vec<Value>> {
            let (message, filename, lineno, colno, slot) = ctx
                .with_instance::<ErrorEvent, _>(event, |event| {
                    (
                        Value::str(&event.message),
                        Value::str(&event.filename),
                        Value::Num(event.lineno as f64),
                        Value::Num(event.colno as f64),
                        event.error_slot.clone(),
                    )
                })
                .ok()?;
            let error = ctx
                .native_private_value_slot(event, &slot)
                .unwrap_or(Value::Undefined);
            Some(vec![message, filename, lineno, colno, error])
        }
    }

    // ---- AbortSignal -------------------------------------------------------------------------

    #[methods]
    impl AbortSignal {
        #[constructor]
        fn constructor() -> OpResult<Self> {
            Err(OpError::type_error("Illegal constructor").with_code("ERR_ILLEGAL_CONSTRUCTOR"))
        }

        #[getter]
        fn aborted(&self) -> bool {
            self.state.aborted.get()
        }

        #[getter]
        fn reason(&self) -> Value {
            self.state.reason.borrow().clone()
        }

        fn throw_if_aborted(&self) -> OpResult<()> {
            if self.state.aborted.get() {
                return Err(OpError::thrown(self.state.reason.borrow().clone()));
            }
            Ok(())
        }

        #[getter]
        fn onabort(ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
            super::super::node::node_handler_get(ctx, &this.0, "abort")
        }

        #[setter]
        fn set_onabort(ctx: &mut Ctx, this: This<Value>, value: Value) -> OpResult<()> {
            super::super::node::node_handler_set(ctx, &this.0, "abort", value)
        }

        fn abort(ctx: &mut Ctx, #[default(Value::Undefined)] reason: Value) -> OpResult<Value> {
            let reason = match reason {
                Value::Undefined => abort::default_reason(ctx),
                reason => reason,
            };
            let signal = abort::new_signal(ctx)?;
            abort::abort_signal(ctx, &signal, reason)?;
            Ok(signal)
        }

        #[method(hint(js(
            missing_message = "The \"delay\" argument must be of type number. Received undefined",
            missing_code = "ERR_INVALID_ARG_TYPE"
        )))]
        fn timeout(ctx: &mut Ctx, delay: Value) -> OpResult<Value> {
            let Value::Num(delay) = delay else {
                return Err(invalid_arg_type(ctx, "delay", "of type number", &delay));
            };
            if delay.fract() != 0.0 || !(0.0..=4294967295.0).contains(&delay) {
                let shown = ctx
                    .coerce_string(&Value::Num(delay))
                    .map_err(OpError::thrown)?;
                return Err(OpError::range_error(format!(
                    "The value of \"delay\" is out of range. It must be >= 0 && <= 4294967295. Received {shown}"
                ))
                .with_code("ERR_OUT_OF_RANGE"));
            }
            abort::timeout_signal(ctx, delay)
        }

        #[method(hint(js(
            missing_message = "The \"signals\" argument must be an instance of Array. Received undefined",
            missing_code = "ERR_INVALID_ARG_TYPE"
        )))]
        fn any(ctx: &mut Ctx, signals: Value) -> OpResult<Value> {
            abort::any_signal(ctx, signals)
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            let fields = vec![("aborted", Value::Bool(self.state.aborted.get()))];
            inspect_object(ctx, &this.0, fields, &depth, &options, &inspect, false)
        }
    }

    impl lumen::embed::NativeIdentityOwner for AbortSignal {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.data.trace_callbacks(visit);
            visit(&self.state.reason.borrow());
            for signal in self.state.sources.borrow().iter() {
                visit(signal);
            }
            for signal in self.state.dependants.borrow().iter() {
                visit(signal);
            }
        }
    }

    // ---- AbortController ---------------------------------------------------------------------

    pub struct AbortControllerConstructor {
        controller: AbortController,
        signal: Value,
    }

    impl CtorRet<JsHost, AbortController> for AbortControllerConstructor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let slot = self.controller.signal_slot.clone();
            let instance = <JsHost as Host>::construct(cx, self.controller)?;
            <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                ctx.define_native_private_value_slot(&instance, &slot, self.signal)
            })?;
            Ok(instance)
        }
    }

    #[methods]
    impl AbortController {
        #[constructor]
        fn constructor(ctx: &mut Ctx) -> OpResult<AbortControllerConstructor> {
            Ok(AbortControllerConstructor {
                signal: abort::new_signal(ctx)?,
                controller: AbortController {
                    signal_slot: ctx.allocate_native_private_slot_name(),
                },
            })
        }

        #[getter]
        fn signal(&self, ctx: &mut Ctx, this: This<Value>) -> Value {
            ctx.native_private_value_slot(&this.0, &self.signal_slot)
                .unwrap_or(Value::Undefined)
        }

        fn abort(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            #[default(Value::Undefined)] reason: Value,
        ) -> OpResult<()> {
            let signal = self.signal(ctx, This(this.0.clone()));
            let reason = match reason {
                Value::Undefined => abort::default_reason(ctx),
                reason => reason,
            };
            abort::abort_signal(ctx, &signal, reason)
        }

        #[method(hint(js(symbol_for = "nodejs.util.inspect.custom")))]
        fn inspect_custom(
            &self,
            ctx: &mut Ctx,
            this: This<Value>,
            depth: Value,
            options: Value,
            inspect: Value,
        ) -> OpResult<Value> {
            let signal = self.signal(ctx, This(this.0.clone()));
            inspect_object(ctx, &this.0, vec![("signal", signal)], &depth, &options, &inspect, false)
        }
    }

    // ---- DOMException ------------------------------------------------------------------------

    /// The legacy code of each DOMException name (`IndexSizeError` is 1).
    const CODES: [&str; 25] = [
        "IndexSizeError",
        "DOMStringSizeError",
        "HierarchyRequestError",
        "WrongDocumentError",
        "InvalidCharacterError",
        "NoDataAllowedError",
        "NoModificationAllowedError",
        "NotFoundError",
        "NotSupportedError",
        "InUseAttributeError",
        "InvalidStateError",
        "SyntaxError",
        "InvalidModificationError",
        "NamespaceError",
        "InvalidAccessError",
        "ValidationError",
        "TypeMismatchError",
        "SecurityError",
        "NetworkError",
        "AbortError",
        "URLMismatchError",
        "QuotaExceededError",
        "TimeoutError",
        "InvalidNodeTypeError",
        "DataCloneError",
    ];

    pub struct DomExceptionConstructor {
        exception: DomException,
        cause: Option<Value>,
    }

    impl CtorRet<JsHost, DomException> for DomExceptionConstructor {
        fn into_ctor(self, cx: &<JsHost as Host>::Cx<'_>) -> Result<Value, Value> {
            let instance = <JsHost as Host>::construct(cx, self.exception)?;
            if let Some(cause) = self.cause {
                <JsHost as Host>::with_ctx(cx, |ctx: &mut Ctx| {
                    let descriptor = ctx.new_object_with_proto(&Value::Null);
                    for (key, value) in [
                        ("value", cause),
                        ("writable", Value::Bool(true)),
                        ("enumerable", Value::Bool(false)),
                        ("configurable", Value::Bool(true)),
                    ] {
                        ctx.member_set(&descriptor, key, value)?;
                    }
                    ctx.define_property_value(&instance, Value::str("cause"), &descriptor)
                })?;
            }
            Ok(instance)
        }
    }

    #[methods]
    impl DomException {
        #[constant(name = "INDEX_SIZE_ERR")]
        const INDEX_SIZE_ERR: u16 = 1;
        #[constant(name = "DOMSTRING_SIZE_ERR")]
        const DOMSTRING_SIZE_ERR: u16 = 2;
        #[constant(name = "HIERARCHY_REQUEST_ERR")]
        const HIERARCHY_REQUEST_ERR: u16 = 3;
        #[constant(name = "WRONG_DOCUMENT_ERR")]
        const WRONG_DOCUMENT_ERR: u16 = 4;
        #[constant(name = "INVALID_CHARACTER_ERR")]
        const INVALID_CHARACTER_ERR: u16 = 5;
        #[constant(name = "NO_DATA_ALLOWED_ERR")]
        const NO_DATA_ALLOWED_ERR: u16 = 6;
        #[constant(name = "NO_MODIFICATION_ALLOWED_ERR")]
        const NO_MODIFICATION_ALLOWED_ERR: u16 = 7;
        #[constant(name = "NOT_FOUND_ERR")]
        const NOT_FOUND_ERR: u16 = 8;
        #[constant(name = "NOT_SUPPORTED_ERR")]
        const NOT_SUPPORTED_ERR: u16 = 9;
        #[constant(name = "INUSE_ATTRIBUTE_ERR")]
        const INUSE_ATTRIBUTE_ERR: u16 = 10;
        #[constant(name = "INVALID_STATE_ERR")]
        const INVALID_STATE_ERR: u16 = 11;
        #[constant(name = "SYNTAX_ERR")]
        const SYNTAX_ERR: u16 = 12;
        #[constant(name = "INVALID_MODIFICATION_ERR")]
        const INVALID_MODIFICATION_ERR: u16 = 13;
        #[constant(name = "NAMESPACE_ERR")]
        const NAMESPACE_ERR: u16 = 14;
        #[constant(name = "INVALID_ACCESS_ERR")]
        const INVALID_ACCESS_ERR: u16 = 15;
        #[constant(name = "VALIDATION_ERR")]
        const VALIDATION_ERR: u16 = 16;
        #[constant(name = "TYPE_MISMATCH_ERR")]
        const TYPE_MISMATCH_ERR: u16 = 17;
        #[constant(name = "SECURITY_ERR")]
        const SECURITY_ERR: u16 = 18;
        #[constant(name = "NETWORK_ERR")]
        const NETWORK_ERR: u16 = 19;
        #[constant(name = "ABORT_ERR")]
        const ABORT_ERR: u16 = 20;
        #[constant(name = "URL_MISMATCH_ERR")]
        const URL_MISMATCH_ERR: u16 = 21;
        #[constant(name = "QUOTA_EXCEEDED_ERR")]
        const QUOTA_EXCEEDED_ERR: u16 = 22;
        #[constant(name = "TIMEOUT_ERR")]
        const TIMEOUT_ERR: u16 = 23;
        #[constant(name = "INVALID_NODE_TYPE_ERR")]
        const INVALID_NODE_TYPE_ERR: u16 = 24;
        #[constant(name = "DATA_CLONE_ERR")]
        const DATA_CLONE_ERR: u16 = 25;

        #[constructor]
        fn constructor(
            ctx: &mut Ctx,
            #[default(Value::Undefined)] message: Value,
            #[default(Value::Undefined)] options: Value,
        ) -> OpResult<DomExceptionConstructor> {
            let message = match message {
                Value::Undefined => String::new(),
                message => ctx.coerce_string(&message).map_err(OpError::thrown)?.to_string(),
            };
            let (name, cause) = match &options {
                Value::Undefined => ("Error".to_string(), None),
                Value::Obj(_) => {
                    let has_cause = ctx
                        .reflect_has(&options, &Value::str("cause"))
                        .map_err(OpError::thrown)?;
                    let cause = if has_cause {
                        Some(ctx.member_get(&options, "cause").map_err(OpError::thrown)?)
                    } else {
                        None
                    };
                    let name = match ctx.member_get(&options, "name").map_err(OpError::thrown)? {
                        Value::Undefined => "Error".to_string(),
                        name => ctx.coerce_string(&name).map_err(OpError::thrown)?.to_string(),
                    };
                    (name, cause)
                }
                other => (
                    ctx.coerce_string(other).map_err(OpError::thrown)?.to_string(),
                    None,
                ),
            };
            Ok(DomExceptionConstructor {
                exception: DomException { name, message },
                cause,
            })
        }

        #[getter]
        fn name(&self) -> String {
            self.name.clone()
        }

        #[getter]
        fn message(&self) -> String {
            self.message.clone()
        }

        #[getter]
        fn code(&self) -> u16 {
            CODES
                .iter()
                .position(|name| *name == self.name)
                .map_or(0, |index| index as u16 + 1)
        }
    }

    impl DomException {
        pub fn with_name(message: &str, name: &str) -> Self {
            Self {
                name: name.into(),
                message: message.into(),
            }
        }

        pub fn exception_name(&self) -> &str {
            &self.name
        }

        pub fn exception_message(&self) -> &str {
            &self.message
        }
    }
}
