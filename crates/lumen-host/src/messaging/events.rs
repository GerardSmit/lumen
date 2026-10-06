//! `MessageEvent`, `CloseEvent` and `PromiseRejectionEvent`.

#[lumen_bind::module(name = "messageEvents")]
pub mod bindings {
    use crate::events::{Event, EventInit};
    use crate::messaging::{frozen_array, inherits_global, is_port, member, Owned};
    use crate::webidl::{invalid_arg_type, usv_string};
    use lumen::embed::{Ctx, NativeIdentityOwner, OpError, OpResult, Value};
    use std::cell::RefCell;

    #[class(name = "MessageEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct MessageEvent {
        base: Event,
        data: RefCell<Value>,
        origin: RefCell<String>,
        last_event_id: RefCell<String>,
        source: RefCell<Value>,
        ports: RefCell<Value>,
    }

    #[class(name = "CloseEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct CloseEvent {
        base: Event,
        was_clean: bool,
        code: u16,
        reason: String,
    }

    #[class(name = "PromiseRejectionEvent", extends = Event, hint(js(webidl, invalid_this)))]
    pub struct PromiseRejectionEvent {
        base: Event,
        promise: Value,
        reason: Value,
    }

    /// The value of a dictionary member that is a `USVString` or `DOMString`, `""` when absent.
    fn text_member(ctx: &mut Ctx, options: &Option<Value>, key: &str, usv: bool) -> OpResult<String> {
        match member(ctx, options, key)? {
            Value::Undefined => Ok(String::new()),
            value if usv => usv_string(ctx, &value).map_err(OpError::thrown),
            value => Ok(ctx.coerce_string(&value).map_err(OpError::thrown)?.to_string()),
        }
    }

    /// `MessageEventSource`: a `MessagePort`, a `Window` or a `ServiceWorker`.
    fn read_source(ctx: &mut Ctx, name: &str, value: Value) -> OpResult<Value> {
        match value {
            Value::Undefined | Value::Null => Ok(Value::Null),
            value if is_port(ctx, &value)
                || inherits_global(ctx, &value, "Window")
                || inherits_global(ctx, &value, "ServiceWorker") =>
            {
                Ok(value)
            }
            value => Err(invalid_arg_type(ctx, name, "an instance of MessagePort", &value)),
        }
    }

    /// `sequence<MessagePort>` as a frozen array.
    fn read_ports(ctx: &mut Ctx, value: Value) -> OpResult<Value> {
        let items = match value {
            Value::Undefined => Vec::new(),
            Value::Obj(_) => ctx
                .iterable_to_list(&value, usize::MAX)
                .map_err(|_| OpError::type_error("ports is not iterable"))?,
            _ => return Err(OpError::type_error("ports is not iterable")),
        };
        for (index, port) in items.iter().enumerate() {
            if !is_port(ctx, port) {
                return Err(invalid_arg_type(
                    ctx,
                    &format!("init.ports[{index}]"),
                    "an instance of MessagePort",
                    port,
                ));
            }
        }
        frozen_array(ctx, items)
    }

    fn undefined_as_null(value: Value) -> Value {
        match value {
            Value::Undefined => Value::Null,
            value => value,
        }
    }

    // ---- MessageEvent ------------------------------------------------------------------------

    #[methods]
    impl MessageEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Owned<Self>> {
            let base = Event::new(ctx, kind, options.clone())?;
            let data = undefined_as_null(member(ctx, &options, "data")?);
            let last_event_id = text_member(ctx, &options, "lastEventId", false)?;
            let origin = text_member(ctx, &options, "origin", true)?;
            let ports = member(ctx, &options, "ports")?;
            let ports = read_ports(ctx, ports)?;
            let source = member(ctx, &options, "source")?;
            let source = read_source(ctx, "init.source", source)?;
            Ok(Owned(Self {
                base,
                data: RefCell::new(data),
                origin: RefCell::new(origin),
                last_event_id: RefCell::new(last_event_id),
                source: RefCell::new(source),
                ports: RefCell::new(ports),
            }))
        }

        #[getter]
        fn data(&self) -> Value {
            self.data.borrow().clone()
        }

        #[getter]
        fn origin(&self) -> String {
            self.origin.borrow().clone()
        }

        #[getter]
        fn last_event_id(&self) -> String {
            self.last_event_id.borrow().clone()
        }

        #[getter]
        fn source(&self) -> Value {
            self.source.borrow().clone()
        }

        #[getter]
        fn ports(&self) -> Value {
            self.ports.borrow().clone()
        }

        #[method(name = "initMessageEvent", coerce)]
        fn init_message_event(
            &self,
            ctx: &mut Ctx,
            kind: &str,
            #[default(false)] bubbles: bool,
            #[default(false)] cancelable: bool,
            #[default(Value::Undefined)] data: Value,
            #[default(Value::Undefined)] origin: Value,
            #[default(Value::Undefined)] last_event_id: Value,
            #[default(Value::Undefined)] source: Value,
            #[default(Value::Undefined)] ports: Value,
        ) -> OpResult<()> {
            let origin = match origin {
                Value::Undefined => String::new(),
                origin => usv_string(ctx, &origin).map_err(OpError::thrown)?,
            };
            let last_event_id = match last_event_id {
                Value::Undefined => String::new(),
                id => ctx.coerce_string(&id).map_err(OpError::thrown)?.to_string(),
            };
            let source = read_source(ctx, "source", source)?;
            let ports = read_ports(ctx, ports)?;
            if !self.base.initialize_legacy(kind, bubbles, cancelable) {
                return Ok(());
            }
            *self.data.borrow_mut() = undefined_as_null(data);
            *self.origin.borrow_mut() = origin;
            *self.last_event_id.borrow_mut() = last_event_id;
            *self.source.borrow_mut() = source;
            *self.ports.borrow_mut() = ports;
            Ok(())
        }
    }

    impl NativeIdentityOwner for MessageEvent {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.trace_native_values(visit);
            visit(&self.data.borrow());
            visit(&self.source.borrow());
            visit(&self.ports.borrow());
        }
    }

    impl MessageEvent {
        /// A trusted `MessageEvent` the user agent fires, with `ports` as its frozen `ports`.
        pub fn create(
            ctx: &mut Ctx,
            kind: &str,
            data: Value,
            origin: &str,
            source: Value,
            ports: Vec<Value>,
        ) -> OpResult<Value> {
            Self::build(ctx, kind, data, origin, "", source, ports)
        }

        /// A trusted `MessageEvent` carrying a `lastEventId` (server-sent events).
        pub fn create_with_id(
            ctx: &mut Ctx,
            kind: &str,
            data: Value,
            origin: &str,
            last_event_id: &str,
        ) -> OpResult<Value> {
            Self::build(ctx, kind, data, origin, last_event_id, Value::Null, Vec::new())
        }

        fn build(
            ctx: &mut Ctx,
            kind: &str,
            data: Value,
            origin: &str,
            last_event_id: &str,
            source: Value,
            ports: Vec<Value>,
        ) -> OpResult<Value> {
            let ports = frozen_array(ctx, ports)?;
            let instance = ctx.new_instance(Self {
                base: Event::trusted(kind),
                data: RefCell::new(data),
                origin: RefCell::new(origin.into()),
                last_event_id: RefCell::new(last_event_id.into()),
                source: RefCell::new(source),
                ports: RefCell::new(ports),
            });
            ctx.set_native_identity_owner::<Self>(&instance)?;
            Ok(instance)
        }
    }

    // ---- CloseEvent --------------------------------------------------------------------------

    fn to_uint16(ctx: &mut Ctx, value: &Value) -> OpResult<u16> {
        if matches!(value, Value::Undefined) {
            return Ok(0);
        }
        let number = ctx.coerce_number(value).map_err(OpError::thrown)?;
        if !number.is_finite() {
            return Ok(0);
        }
        Ok(number.trunc().rem_euclid(65536.0) as u16)
    }

    #[methods]
    impl CloseEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" argument must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, options: Option<Value>) -> OpResult<Self> {
            let base = Event::new(ctx, kind, options.clone())?;
            let code = member(ctx, &options, "code")?;
            let code = to_uint16(ctx, &code)?;
            let reason = text_member(ctx, &options, "reason", true)?;
            let was_clean = member(ctx, &options, "wasClean")?;
            let was_clean = ctx.to_boolean(&was_clean);
            Ok(Self {
                base,
                was_clean,
                code,
                reason,
            })
        }

        #[getter]
        fn was_clean(&self) -> bool {
            self.was_clean
        }

        #[getter]
        fn code(&self) -> u16 {
            self.code
        }

        #[getter]
        fn reason(&self) -> String {
            self.reason.clone()
        }
    }

    impl CloseEvent {
        /// A trusted `CloseEvent` the user agent fires.
        pub fn create(ctx: &mut Ctx, kind: &str, code: u16, reason: &str, was_clean: bool) -> Value {
            ctx.new_instance(Self {
                base: Event::trusted(kind),
                was_clean,
                code,
                reason: reason.into(),
            })
        }
    }

    // ---- PromiseRejectionEvent ---------------------------------------------------------------

    #[methods]
    impl PromiseRejectionEvent {
        #[constructor(
            coerce,
            hint(js(
                missing_message = "The \"type\" and \"eventInitDict\" arguments must be specified",
                missing_code = "ERR_MISSING_ARGS"
            ))
        )]
        fn constructor(ctx: &mut Ctx, kind: &str, init: Value) -> OpResult<Owned<Self>> {
            if !matches!(init, Value::Obj(_)) {
                return Err(invalid_arg_type(ctx, "eventInitDict", "of type object", &init));
            }
            let options = Some(init);
            let base = Event::new(ctx, kind, options.clone())?;
            let promise = member(ctx, &options, "promise")?;
            if matches!(promise, Value::Undefined) {
                return Err(OpError::type_error(
                    "PromiseRejectionEvent requires an init with a promise",
                ));
            }
            let promise = ctx.coerce_promise(promise)?;
            let reason = member(ctx, &options, "reason")?;
            Ok(Owned(Self { base, promise, reason }))
        }

        #[getter]
        fn promise(&self) -> Value {
            self.promise.clone()
        }

        #[getter]
        fn reason(&self) -> Value {
            self.reason.clone()
        }
    }

    impl NativeIdentityOwner for PromiseRejectionEvent {
        const TRACES_NATIVE_VALUES: bool = true;

        fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

        fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
            self.base.trace_native_values(visit);
            visit(&self.promise);
            visit(&self.reason);
        }
    }

    impl PromiseRejectionEvent {
        /// A trusted event the user agent fires for an unhandled or handled rejection:
        /// `unhandledrejection` is cancelable.
        pub fn for_user_agent(
            ctx: &mut Ctx,
            kind: &str,
            promise: Value,
            reason: Value,
        ) -> OpResult<Value> {
            let base = Event::from_init(
                kind,
                EventInit {
                    cancelable: kind == "unhandledrejection",
                    trusted: true,
                    ..EventInit::default()
                },
            );
            let instance = ctx.new_instance(Self { base, promise, reason });
            ctx.set_native_identity_owner::<Self>(&instance)?;
            Ok(instance)
        }
    }
}
