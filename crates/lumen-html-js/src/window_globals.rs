use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::Promise;
use lumen_bind::OneOrNumberPair;

fn scroll_window_receiver(
    ctx: &mut Ctx,
    receiver: &Value,
    args: OneOrNumberPair<scrolling::ScrollToOptions>,
    relative: bool,
) -> OpResult<Promise<()>> {
    // The browser global is published through its WindowProxy. Generated
    // `&DomWindow` projection only recognizes a WindowProxy while an internal
    // property Get/Set scope is active; a method call occurs after that scope
    // has ended. Resolve the actual receiver through the checked native Window
    // path so its brand and original-caller security policy are preserved.
    let realm = ctx
        .with_instance::<DomWindow, _>(receiver, |window| window.base.associated_realm())?
        .ok_or_else(|| OpError::new("InvalidStateError", "Window document is unavailable"))?;
    let node = realm.session.borrow().document().root();
    Ok(scroll_node(ctx, realm, node, args, relative))
}

pub(crate) fn scroll_node(
    ctx: &mut Ctx,
    realm: Rc<DomRealm>,
    node: NodeId,
    args: OneOrNumberPair<scrolling::ScrollToOptions>,
    relative: bool,
) -> Promise<()> {
    let (current_x, current_y) = match scrolling::position(&realm, node) {
        Ok(position) => position,
        Err(error) => return Promise::rejected(error),
    };
    let (x, y, behavior) = match args {
        OneOrNumberPair::Pair(x, y) if relative => (
            current_x + x,
            current_y + y,
            scrolling::ScrollBehavior::Auto,
        ),
        OneOrNumberPair::Pair(x, y) => (x, y, scrolling::ScrollBehavior::Auto),
        OneOrNumberPair::One(options) if relative => (
            current_x + options.left.unwrap_or(0.0),
            current_y + options.top.unwrap_or(0.0),
            options.behavior,
        ),
        OneOrNumberPair::One(options) => (
            options.left.unwrap_or(current_x),
            options.top.unwrap_or(current_y),
            options.behavior,
        ),
    };
    scrolling::set_offset(ctx, &realm, node, x, y, behavior)
}

struct WindowRealm(std::rc::Weak<DomRealm>);

pub(crate) fn current_dom_realm(ctx: &mut Ctx) -> Option<Rc<DomRealm>> {
    RealmServices::<WindowRealm>::current(ctx).and_then(|state| state.0.upgrade())
}

#[lumen_bind::class(name = "Window", extends = DomEventTarget, hint(js(webidl)))]
pub(crate) struct DomWindow {
    base: DomEventTarget,
}

impl DomWindow {
    pub(crate) fn from_target(base: DomEventTarget) -> Self {
        Self { base }
    }

    fn viewport_size(&self) -> OpResult<(u32, u32)> {
        let Some(realm) = self.base.associated_realm() else {
            return Ok((0, 0));
        };
        if realm.layout_flusher.borrow().is_none()
            && realm.session.borrow().viewport_size().is_none()
        {
            return Ok((0, 0));
        }
        realm.flush_layout()?;
        let size = realm.session.borrow().viewport_size().unwrap_or((0, 0));
        Ok(size)
    }

    fn scroll_position(&self) -> OpResult<(f64, f64)> {
        let Some(realm) = self.base.associated_realm() else {
            return Ok((0.0, 0.0));
        };
        let node = realm.session.borrow().document().root();
        scrolling::position(&realm, node)
    }
}

#[lumen_bind::methods]
impl DomWindow {
    #[getter]
    fn inner_width(&self) -> OpResult<u32> {
        self.viewport_size().map(|size| size.0)
    }

    #[getter]
    fn inner_height(&self) -> OpResult<u32> {
        self.viewport_size().map(|size| size.1)
    }

    #[getter]
    fn scroll_x(&self) -> OpResult<f64> {
        self.scroll_position().map(|position| position.0)
    }

    #[getter]
    fn scroll_y(&self) -> OpResult<f64> {
        self.scroll_position().map(|position| position.1)
    }

    #[getter]
    fn page_x_offset(&self) -> OpResult<f64> {
        self.scroll_x()
    }

    #[getter]
    fn page_y_offset(&self) -> OpResult<f64> {
        self.scroll_y()
    }

    #[method(coerce)]
    fn scroll(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[varargs] args: OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> OpResult<Promise<()>> {
        scroll_window_receiver(ctx, &this.0, args, false)
    }

    #[method(coerce)]
    fn scroll_to(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[varargs] args: OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> OpResult<Promise<()>> {
        scroll_window_receiver(ctx, &this.0, args, false)
    }

    #[method(coerce)]
    fn scroll_by(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        #[varargs] args: OneOrNumberPair<scrolling::ScrollToOptions>,
    ) -> OpResult<Promise<()>> {
        scroll_window_receiver(ctx, &this.0, args, true)
    }

    #[getter]
    fn parent(&self, ctx: &mut Ctx) -> Value {
        match browsing_context::current_realm_context(ctx) {
            Some(context) => browsing_context::window_parent_value(ctx, &context),
            None => browsing_context::current_realm_metadata(ctx)
                .map(|metadata| browsing_context::window_parent_from_metadata(ctx, &metadata))
                .unwrap_or_else(|| ctx.global_this_value()),
        }
    }

    #[getter]
    fn top(&self, ctx: &mut Ctx) -> Value {
        match browsing_context::current_realm_context(ctx) {
            Some(context) => browsing_context::window_top_value(ctx, &context),
            None => browsing_context::current_realm_metadata(ctx)
                .map(|metadata| browsing_context::window_top_from_metadata(ctx, &metadata))
                .unwrap_or_else(|| ctx.global_this_value()),
        }
    }

    #[getter]
    fn frames(&self, ctx: &mut Ctx) -> Value {
        match browsing_context::current_realm_context(ctx) {
            Some(context) => browsing_context::window_frames_value(ctx, &context),
            None => browsing_context::current_realm_metadata(ctx)
                .map(|metadata| browsing_context::window_self_from_metadata(ctx, &metadata))
                .unwrap_or_else(|| ctx.global_this_value()),
        }
    }

    #[getter]
    fn length(&self, ctx: &mut Ctx) -> u32 {
        browsing_context::current_realm_context(ctx)
            .map_or(0, |context| browsing_context::window_length(&context))
    }

    #[getter]
    fn location(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let context = browsing_context::current_realm_context(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window has no active context"))?;
        let realm = RealmServices::<WindowRealm>::current(ctx)
            .and_then(|state| state.0.upgrade())
            .ok_or_else(|| OpError::new("InvalidStateError", "Window document is unavailable"))?;
        location_value(ctx, &realm, &context)
    }

    #[setter(coerce)]
    fn set_location(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        let context = browsing_context::current_realm_context(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window has no active context"))?;
        let entry_base = entry_base_url(ctx, &context);
        context.request_location_navigation_from(value, &entry_base)
    }

    #[getter(name = "frameElement")]
    fn frame_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let Some(context) = browsing_context::current_realm_context(ctx) else {
            return match browsing_context::current_realm_metadata(ctx) {
                Some(metadata) => {
                    browsing_context::window_frame_element_from_metadata(ctx, &metadata)
                }
                None => Ok(Value::Null),
            };
        };
        browsing_context::window_frame_element(ctx, &context)
    }

    #[getter]
    fn onerror(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "error")
    }

    #[setter]
    fn set_onerror(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "error", callback);
    }

    #[getter]
    fn onload(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "load")
    }

    #[setter]
    fn set_onload(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base.set_handler(ctx, &this.0, "load", callback);
    }

    #[getter]
    fn onunhandledrejection(&self) -> Nullable<lumen::embed::JsFunction> {
        Nullable(self.base.handler("unhandledrejection"))
    }

    #[setter]
    fn set_onunhandledrejection(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .set_handler(ctx, &this.0, "unhandledrejection", callback);
    }

    #[getter]
    fn onrejectionhandled(&self) -> Nullable<lumen::embed::JsFunction> {
        Nullable(self.base.handler("rejectionhandled"))
    }

    #[setter]
    fn set_onrejectionhandled(
        &self,
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        callback: Option<lumen::embed::JsFunction>,
    ) {
        self.base
            .set_handler(ctx, &this.0, "rejectionhandled", callback);
    }
}

#[lumen_bind::class(name = "Location", hint(js(webidl)))]
pub(crate) struct DomLocation {
    context: std::rc::Weak<browsing_context::BrowsingContext>,
    owner: std::rc::Weak<DomRealm>,
}

impl DomLocation {
    fn active_owner_and_context(
        &self,
    ) -> OpResult<(Rc<DomRealm>, Rc<browsing_context::BrowsingContext>)> {
        let context = self
            .context
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "Location context is unavailable"))?;
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| OpError::new("InvalidStateError", "Location document is unavailable"))?;
        let current = browsing_context::context_document(&context);
        if !current
            .as_ref()
            .is_some_and(|current| Rc::ptr_eq(current, &owner))
        {
            return Err(OpError::new(
                "InvalidStateError",
                "Location belongs to a retired document",
            ));
        }
        Ok((owner, context))
    }

    fn url(&self) -> OpResult<lumen_common::url::Url> {
        let (owner, _) = self.active_owner_and_context()?;
        let url = owner
            .document_url()
            .unwrap_or_else(|| "about:blank".to_owned());
        lumen_common::url::parse(&url, None)
            .map_err(|_| OpError::new("InvalidStateError", "document URL is invalid"))
    }

    fn navigate(&self, ctx: &mut Ctx, input: &str) -> OpResult<()> {
        let (_, context) = self.active_owner_and_context()?;
        let entry_base = entry_base_url(ctx, &context);
        context.request_location_navigation_from(input, &entry_base)
    }

    fn update(
        &self,
        ctx: &mut Ctx,
        change: impl FnOnce(&mut lumen_common::url::Url) -> bool,
    ) -> OpResult<()> {
        let mut url = self.url()?;
        if change(&mut url) {
            self.navigate(ctx, &url.href())?;
        }
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomLocation {
    #[getter]
    fn href(&self) -> OpResult<String> {
        Ok(self.url()?.href())
    }

    #[setter(coerce)]
    fn set_href(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.navigate(ctx, value)
    }

    #[getter]
    fn origin(&self) -> OpResult<String> {
        let (_, context) = self.active_owner_and_context()?;
        Ok(browsing_context::context_origin(&context).serialize())
    }

    #[getter]
    fn protocol(&self) -> OpResult<String> {
        Ok(format!("{}:", self.url()?.scheme))
    }

    #[setter(coerce)]
    fn set_protocol(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| url.set_protocol(value))
    }

    #[getter]
    fn host(&self) -> OpResult<String> {
        let url = self.url()?;
        let mut host = url.host.unwrap_or_default();
        if let Some(port) = url.port {
            host.push(':');
            host.push_str(&port.to_string());
        }
        Ok(host)
    }

    #[setter(coerce)]
    fn set_host(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| url.set_host(value))
    }

    #[getter]
    fn hostname(&self) -> OpResult<String> {
        Ok(self.url()?.host.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_hostname(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| url.set_hostname(value))
    }

    #[getter]
    fn port(&self) -> OpResult<String> {
        Ok(self
            .url()?
            .port
            .map(|port| port.to_string())
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_port(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| url.set_port(value))
    }

    #[getter]
    fn pathname(&self) -> OpResult<String> {
        Ok(self.url()?.path)
    }

    #[setter(coerce)]
    fn set_pathname(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| url.set_pathname(value))
    }

    #[getter]
    fn search(&self) -> OpResult<String> {
        Ok(self
            .url()?
            .query
            .map(|query| format!("?{query}"))
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_search(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| {
            url.set_search(value);
            true
        })
    }

    #[getter]
    fn hash(&self) -> OpResult<String> {
        Ok(self
            .url()?
            .fragment
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_hash(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        self.update(ctx, |url| {
            url.set_hash(value);
            true
        })
    }

    #[method]
    fn assign(&self, ctx: &mut Ctx, url: &str) -> OpResult<()> {
        self.navigate(ctx, url)
    }

    #[method]
    fn replace(&self, ctx: &mut Ctx, url: &str) -> OpResult<()> {
        self.navigate(ctx, url)
    }

    #[method]
    fn reload(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.navigate(ctx, &self.url()?.href())
    }

    #[method(name = "toString")]
    fn to_string(&self) -> OpResult<String> {
        self.href()
    }
}

pub(crate) fn location_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    context: &Rc<browsing_context::BrowsingContext>,
) -> OpResult<Value> {
    if let Some(location) = realm
        .location_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
    {
        return Ok(location);
    }
    let location = ctx.new_instance(DomLocation {
        context: Rc::downgrade(context),
        owner: Rc::downgrade(realm),
    });
    *realm.location_wrapper.borrow_mut() = ctx.weak_value(&location);
    Ok(location)
}

pub(crate) fn entry_base_url(
    ctx: &mut Ctx,
    target: &Rc<browsing_context::BrowsingContext>,
) -> String {
    let caller = ctx.invocation_host_realm();
    ctx.with_host_realm(&caller, |ctx| {
        RealmServices::<WindowRealm>::current(ctx)
            .and_then(|state| state.0.upgrade())
            .map(|realm| realm.base_url())
    })
    .ok()
    .flatten()
    .unwrap_or_else(|| target.current_document_url())
}

#[lumen_bind::op(name = "getSelection")]
fn get_selection(ctx: &mut Ctx) -> Value {
    let realm = RealmServices::<WindowRealm>::current(ctx).and_then(|state| state.0.upgrade());
    realm.map_or(Value::Null, |realm| realm.selection_value(ctx))
}

#[lumen_bind::class(name = "WindowNamedPropertiesHandler")]
struct WindowNamedPropertiesHandler {
    realm: std::rc::Weak<DomRealm>,
    global: WeakValue,
}

impl WindowNamedPropertiesHandler {
    fn named_nodes(&self, name: &str) -> OpResult<Vec<NodeId>> {
        let Some(realm) = self.realm.upgrade() else {
            return Ok(Vec::new());
        };
        let session = realm.session.borrow();
        let document = session.document();
        let root = document.root();
        let filter = DescendantFilter::WindowNamed(name.to_owned());
        let mut nodes = Vec::new();
        let mut next = super::next_descendant(document, root, root).map_err(super::dom_error)?;
        while let Some(node) = next {
            if filter.matches(document, node).map_err(super::dom_error)? {
                nodes.push(node);
            }
            next = super::next_descendant(document, root, node).map_err(super::dom_error)?;
        }
        Ok(nodes)
    }

    /// Window's named-properties object is below the global's own properties and does not
    /// override a property already present on the global's prior prototype chain.
    fn named_property_visible(&self, ctx: &mut Ctx, target: &Value, name: &str) -> OpResult<bool> {
        if self.named_nodes(name)?.is_empty() {
            return Ok(false);
        }
        let Some(global) = self.global.upgrade() else {
            return Ok(false);
        };
        let own = ctx
            .reflect_get_own_property_descriptor(&global, &Value::str(name))
            .map_err(OpError::thrown)?;
        if !matches!(own, Value::Undefined) {
            return Ok(false);
        }
        let inherited = ctx
            .reflect_has(target, &Value::str(name))
            .map_err(OpError::thrown)?;
        Ok(!inherited)
    }

    fn named_value(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        let Some(realm) = self.realm.upgrade() else {
            return Ok(Value::Undefined);
        };
        let nodes = self.named_nodes(name)?;
        match nodes.as_slice() {
            [] => Ok(Value::Undefined),
            [node] => Ok(realm.wrap(ctx, *node)),
            _ => {
                let root = realm.session.borrow().document().root();
                let owner = self.global.upgrade().unwrap_or(Value::Undefined);
                Ok(ctx.new_instance(DomHtmlCollection {
                    base: DomNodeList::descendants(
                        realm,
                        root,
                        DescendantFilter::WindowNamed(name.to_owned()),
                        owner,
                    ),
                }))
            }
        }
    }
}

#[lumen_bind::methods]
impl WindowNamedPropertiesHandler {
    #[method(name = "has")]
    fn has_property(&self, ctx: &mut Ctx, target: Value, key: Value) -> OpResult<Value> {
        if let Value::Str(name) = &key {
            if self.named_property_visible(ctx, &target, name.as_str())? {
                return Ok(Value::Bool(true));
            }
        }
        Ok(Value::Bool(
            ctx.reflect_has(&target, &key).map_err(OpError::thrown)?,
        ))
    }

    #[method(name = "get")]
    fn get_property(
        &self,
        ctx: &mut Ctx,
        target: Value,
        key: Value,
        receiver: Value,
    ) -> OpResult<Value> {
        if let Value::Str(name) = &key {
            if self.named_property_visible(ctx, &target, name.as_str())? {
                return self.named_value(ctx, name.as_str());
            }
        }
        ctx.reflect_get(&target, &key, &receiver)
            .map_err(OpError::thrown)
    }

    #[method(name = "getOwnPropertyDescriptor")]
    fn get_own_property_descriptor(
        &self,
        ctx: &mut Ctx,
        target: Value,
        key: Value,
    ) -> OpResult<Value> {
        if let Value::Str(name) = &key {
            if self.named_property_visible(ctx, &target, name.as_str())? {
                let descriptor = Value::Obj(ctx.new_object());
                for (property, value) in [
                    ("value", self.named_value(ctx, name.as_str())?),
                    ("writable", Value::Bool(true)),
                    ("enumerable", Value::Bool(false)),
                    ("configurable", Value::Bool(true)),
                ] {
                    ctx.member_set(&descriptor, property, value)?;
                }
                return Ok(descriptor);
            }
        }
        ctx.reflect_get_own_property_descriptor(&target, &key)
            .map_err(OpError::thrown)
    }
}

fn install_named_properties(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    let object = ctx.member_get(&global, "Object").map_err(OpError::thrown)?;
    let get_prototype_of = ctx
        .member_get(&object, "getPrototypeOf")
        .map_err(OpError::thrown)?;
    let set_prototype_of = ctx
        .member_get(&object, "setPrototypeOf")
        .map_err(OpError::thrown)?;

    let previous_prototype = ctx
        .invoke(
            get_prototype_of,
            object.clone(),
            std::slice::from_ref(&global),
        )
        .map_err(OpError::thrown)?;
    let target = ctx.new_object_with_proto(&previous_prototype);
    let handler = ctx.new_instance(WindowNamedPropertiesHandler {
        realm: Rc::downgrade(realm),
        global: ctx.weak_value(&global).expect("window global is an object"),
    });
    let named_properties = ctx.create_proxy(target, handler).map_err(OpError::thrown)?;
    ctx.invoke(set_prototype_of, object, &[global, named_properties])
        .map_err(OpError::thrown)?;
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    RealmServices::replace_current(ctx, WindowRealm(Rc::downgrade(realm)));
    let function = ctx.bound_function(&lumen_bind::FnItem::of::<get_selection::Op>());
    let global = ctx.global_object();
    // The general runtime exposes data properties for its Node-style error shim.
    // Transfer existing callbacks to the native Window handler slots so those
    // properties cannot shadow the browser's event-handler accessors.
    for name in ["onerror", "onunhandledrejection"] {
        if ctx
            .has_own_property_value(&global, &Value::str(name))
            .map_err(OpError::thrown)?
        {
            let callback = ctx.member_get(&global, name).map_err(OpError::thrown)?;
            if !ctx.delete_member(&global, name).map_err(OpError::thrown)? {
                return Err(OpError::type_error(
                    "runtime event handler property is not configurable",
                ));
            }
            let callback = if callback.is_callable() {
                callback
            } else {
                Value::Null
            };
            ctx.member_set(&global, name, callback)
                .map_err(OpError::thrown)?;
        }
    }
    ctx.set_member(&global, "getSelection", function)
        .map_err(|_| OpError::new("Error", "getSelection install failed"))?;
    install_named_properties(ctx, realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn script(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid script") {
            Ok(value) => value,
            Err(error) => {
                let message = engine
                    .ctx()
                    .get_member(&error, "message")
                    .ok()
                    .and_then(|value| match value {
                        Value::Str(message) => Some(message.to_string()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "script threw".into());
                panic!("{message}");
            }
        }
    }

    #[test]
    fn native_performance_is_an_event_target_of_the_window_realm() {
        let mut engine = Engine::new();
        lumen_host::install(
            &mut engine,
            &[lumen_host::Extension {
                modules: &[lumen_host::performance::install_globals],
                ..lumen_host::Extension::new("performance")
            }],
        );
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(
            &mut engine,
            r#"
            let rejected = false;
            try { new Performance(); } catch(error) { rejected = error instanceof TypeError; }
            let count = 0;
            performance.addEventListener('clock', () => ++count, {once:true});
            performance.dispatchEvent(new Event('clock'));
            performance.dispatchEvent(new Event('clock'));
            rejected && Performance.length === 0 && count === 1 &&
              Object.getPrototypeOf(Performance) === EventTarget &&
              Object.getPrototypeOf(Performance.prototype) === EventTarget.prototype &&
              performance instanceof Performance && performance instanceof EventTarget &&
              Object.getOwnPropertyDescriptor(globalThis,'Performance').enumerable === false &&
              Object.prototype.toString.call(performance) === '[object Performance]' &&
              Object.getOwnPropertyDescriptor(Performance.prototype,'now').enumerable === true &&
              typeof performance.now() === 'number' && performance.toJSON().timeOrigin > 0
        "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn window_named_properties_are_live_and_respect_window_precedence() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<main><div id='target'></div><div id='duplicate'></div><b id='duplicate'></b><div id='other' name='notEligible'></div><img name='namedImage'><div id='document'></div><div id='addEventListener'></div></main>",
            96,
        )
        .unwrap();

        let result = script(
            &mut engine,
            r#"
            const targetNode = document.getElementById('target');
            const duplicateNodes = window.duplicate;
            const initial = target === targetNode && duplicateNodes instanceof HTMLCollection &&
              duplicateNodes.length === 2 && duplicateNodes[0] === document.getElementById('duplicate');
            const namedImageNode = document.querySelector('img');
            const eligibleName = window.namedImage === namedImageNode;
            const nameIsRestricted = typeof notEligible === 'undefined';
            const builtInWins = window.document === document &&
              typeof window.addEventListener === 'function' &&
              window.addEventListener !== document.getElementById('addEventListener');
            window.ownShadow = 'window-own';
            const ownShadowNode = document.createElement('div');
            ownShadowNode.id = 'ownShadow';
            document.body.appendChild(ownShadowNode);
            const ownPropertyWins = window.ownShadow === 'window-own' && ownShadow === 'window-own';
            const namedPrototype = Object.getPrototypeOf(window);
            const namedDescriptor = Object.getOwnPropertyDescriptor(namedPrototype, 'duplicate');
            const prototypeBehavior = Object.prototype.hasOwnProperty.call(namedPrototype, 'duplicate') &&
              namedDescriptor.enumerable === false &&
              !Object.prototype.hasOwnProperty.call(window, 'duplicate') &&
              !Object.keys(window).includes('duplicate');

            const inserted = document.createElement('i');
            inserted.id = 'appearsOnInsert';
            const absentBeforeInsert = typeof appearsOnInsert === 'undefined';
            document.body.appendChild(inserted);
            const appearsAfterInsert = appearsOnInsert === inserted;
            document.body.removeChild(inserted);
            const disappearsAfterRemoval = typeof appearsOnInsert === 'undefined';

            const renamed = targetNode;
            renamed.id = 'renamedTarget';
            const followsIdMutation = typeof target === 'undefined' && renamedTarget === renamed;

            const third = document.createElement('i');
            third.id = 'duplicate';
            document.body.appendChild(third);
            const collectionIsLive = duplicateNodes.length === 3 && duplicateNodes[2] === third;
            third.id = 'notADuplicate';
            const collectionTracksRename = duplicateNodes.length === 2;

            const adopted = document.createElement('div');
            adopted.id = 'adoptedAway';
            document.body.appendChild(adopted);
            const otherDocument = document.implementation.createHTMLDocument('other');
            otherDocument.adoptNode(adopted);
            const adoptionRemovesName = typeof adoptedAway === 'undefined';
            document.adoptNode(adopted);
            document.body.appendChild(adopted);
            const adoptionBackRestoresName = adoptedAway === adopted;

            initial && eligibleName && nameIsRestricted && builtInWins && ownPropertyWins &&
              prototypeBehavior &&
              absentBeforeInsert && appearsAfterInsert && disappearsAfterRemoval &&
              followsIdMutation && collectionIsLive && collectionTracksRename &&
              adoptionRemovesName && adoptionBackRestoresName
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn window_named_properties_survive_mutable_reflect_and_proxy_globals() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<body><img name='one'><div id='many'></div><b id='many'></b></body>",
            64,
        )
        .unwrap();
        let result = script(
            &mut engine,
            r#"
                globalThis.Reflect = {
                    get() { throw new Error('mutable Reflect.get used'); },
                    has() { throw new Error('mutable Reflect.has used'); },
                    getOwnPropertyDescriptor() { throw new Error('mutable Reflect descriptor used'); }
                };
                globalThis.Proxy = function() { throw new Error('mutable Proxy used'); };
                const image = document.querySelector('img');
                const initial = window.one === image &&
                    window.many instanceof HTMLCollection && window.many.length === 2;
                const added = document.createElement('i');
                added.id = 'liveAfterReplacement';
                document.body.appendChild(added);
                const singleIsLive = window.liveAfterReplacement === added;
                const duplicate = document.createElement('i');
                duplicate.id = 'many';
                document.body.appendChild(duplicate);
                const collectionIsLive = window.many.length === 3 && window.many[2] === duplicate;
                initial && singleIsLive && collectionIsLive && window.document === document
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn native_window_global_bindings_resolve_the_active_host_realm() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        crate::install(ctx, "<main></main>", 32).unwrap();
        let parent_global = ctx.global_object();
        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "window.__parentSelection = getSelection(); \
                 window === globalThis && document.defaultView === window && \
                 __parentSelection === getSelection() && \
                 __parentSelection === window.getSelection()",
            ),
            Ok(Value::Bool(true))
        ));

        let child_handle = ctx.create_host_realm();
        let child_global = child_handle.global();
        ctx.with_host_realm(&child_handle, |ctx| {
            crate::install(ctx, "<main></main>", 32).unwrap();
            assert!(matches!(
                ctx.eval_in_realm(
                    &child_global,
                    "window.__childSelection = getSelection(); \
                     window === globalThis && document.defaultView === window && \
                     __childSelection === getSelection() && \
                     __childSelection === window.getSelection()",
                ),
                Ok(Value::Bool(true))
            ));
        })
        .expect("install the child window realm");

        assert!(matches!(
            ctx.eval_in_realm(
                &parent_global,
                "__parentSelection === getSelection() && \
                 document.defaultView === window",
            ),
            Ok(Value::Bool(true))
        ));
        assert!(matches!(
            ctx.eval_in_realm(
                &child_global,
                "__childSelection === getSelection() && \
                 document.defaultView === window",
            ),
            Ok(Value::Bool(true))
        ));
    }

    #[test]
    fn location_is_document_backed_and_rejects_unsupported_top_level_navigation() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<main></main>", 32).unwrap();
        realm.set_document_url("https://origin.example.test/dir/page.html?x=1#top");
        let result = script(
            &mut engine,
            r#"
            const locationIsShared = window.location === document.location;
            const initial = location.href === document.URL &&
              location.href === 'https://origin.example.test/dir/page.html?x=1#top' &&
              location.origin === 'https://origin.example.test' &&
              location.pathname === '/dir/page.html' && location.search === '?x=1' && location.hash === '#top';
            let rejected = false;
            try { location.assign('/next.html'); }
            catch (error) { rejected = error.name === 'NotSupportedError'; }
            locationIsShared && initial && rejected && location.href === document.URL
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn window_scroll_api_uses_root_session_and_typed_overloads() {
        struct NoText;
        impl lumen_html::paint::TextShaper for NoText {
            fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
                Err(())
            }
            fn ascent(&self, size: f32) -> f32 {
                size * 0.8
            }
            fn line_height(&self, size: f32) -> f32 {
                size * 1.2
            }
        }

        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>html,body{margin:0}body{width:400px;height:300px}</style><main></main>",
            32,
        )
        .unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));

        let result = script(
            &mut engine,
            r#"
            const same = () => window.scrollX === window.pageXOffset &&
              window.scrollY === window.pageYOffset;
            if (!same() || window.scrollX !== 0 || window.scrollY !== 0)
              throw new Error('initial viewport offset aliases');

            let conversion = [];
            const options = {
              get behavior() { conversion.push('behavior'); return 'instant'; },
              get left() { conversion.push('left'); return { valueOf() { conversion.push('left-number'); return 30; } }; },
              get top() { conversion.push('top'); return { valueOf() { conversion.push('top-number'); return 40; } }; }
            };
            const first = window.scroll(options);
            if (!(first instanceof Promise) || conversion.join(',') !== 'behavior,left,left-number,top,top-number' ||
                window.scrollX !== 30 || window.scrollY !== 40 || !same())
              throw new Error('dictionary conversion order or scroll()');

            const second = window.scrollTo(10, 15);
            if (!(second instanceof Promise) || window.scrollX !== 10 || window.scrollY !== 15)
              throw new Error('two-number scrollTo overload');
            const third = window.scrollBy(5, 6);
            if (!(third instanceof Promise) || window.scrollX !== 15 || window.scrollY !== 21)
              throw new Error('two-number scrollBy overload');

            const numericOrder = [];
            window.scrollTo(
              { valueOf() { numericOrder.push('x'); return 20; } },
              { valueOf() { numericOrder.push('y'); return 25; } },
              { valueOf() { throw new Error('extra argument converted'); } });
            if (numericOrder.join(',') !== 'x,y' || window.scrollX !== 20 || window.scrollY !== 25)
              throw new Error('coordinate conversion order');
            window.scrollTo('15', '21');
            if (window.scrollX !== 15 || window.scrollY !== 21)
              throw new Error('coordinate string conversion');
            window.scroll();
            if (window.scrollX !== 15 || window.scrollY !== 21 || window.scroll.length !== 0)
              throw new Error('missing dictionary or Web IDL arity');
            let invalidDictionary = false;
            try { window.scrollTo(20); } catch(error) { invalidDictionary = error.name === 'TypeError'; }
            if (!invalidDictionary) throw new Error('primitive dictionary accepted');

            window.scrollTo({left: 12});
            if (window.scrollX !== 12 || window.scrollY !== 21)
              throw new Error('omitted absolute axis must preserve its current position');
            window.scrollBy({top: 9});
            if (window.scrollX !== 12 || window.scrollY !== 30)
              throw new Error('omitted relative axis must contribute zero');
            window.scrollTo(Infinity, NaN);
            if (window.scrollX !== 0 || window.scrollY !== 0)
              throw new Error('non-finite scroll coordinates must normalize to zero');
            window.scrollTo(10000, 10000);
            if (!same() || window.scrollX > 1000 || window.scrollY > 1000)
              throw new Error('root scroll offsets must be clamped to layout extent');
            true
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn window_scroll_methods_use_the_receiver_window_realm() {
        struct NoText;
        impl lumen_html::paint::TextShaper for NoText {
            fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
                Err(())
            }
            fn ascent(&self, size: f32) -> f32 {
                size * 0.8
            }
            fn line_height(&self, size: f32) -> f32 {
                size * 1.2
            }
        }

        let mut engine = Engine::new();
        let parent = crate::install(
            engine.ctx(),
            "<iframe id='child' srcdoc=\"<style>html,body{margin:0}body{height:300px}</style>\"></iframe>",
            32,
        )
        .unwrap();
        let iframe = {
            let session = parent.session.borrow();
            lumen_html::selector::get_element_by_id(
                session.document(),
                session.document().root(),
                "child",
            )
            .unwrap()
            .unwrap()
        };
        let frame = parent
            .ensure_frame_context(engine.ctx(), iframe)
            .expect("create the same-origin iframe realm");
        let child = frame.current_document().expect("install srcdoc");
        child.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));

        let result = script(
            &mut engine,
            r#"
            const childWindow = document.getElementById('child').contentWindow;
            const borrowedScrollTo = window.scrollTo;
            borrowedScrollTo.call(childWindow, 0, 25);
            const foreignWindowReceiver = childWindow.scrollY === 25 && window.scrollY === 0;
            let rejectsUnbrandedReceiver = false;
            try { borrowedScrollTo.call({}, 0, 40); }
            catch (error) { rejectsUnbrandedReceiver = error instanceof TypeError; }
            foreignWindowReceiver && rejectsUnbrandedReceiver
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn element_scroll_methods_update_only_the_receiving_node() {
        struct NoText;
        impl lumen_html::paint::TextShaper for NoText {
            fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> {
                Err(())
            }
            fn ascent(&self, size: f32) -> f32 {
                size * 0.8
            }
            fn line_height(&self, size: f32) -> f32 {
                size * 1.2
            }
        }

        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<style>html,body{margin:0}#scroller{width:20px;height:20px;overflow:auto}#content{width:200px;height:200px}</style><div id=scroller><div id=content></div></div>",
            32,
        )
        .unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(100, 80, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));

        let result = script(
            &mut engine,
            r#"
            const scroller = document.getElementById('scroller');
            const first = scroller.scrollTo(12, 15);
            if (!(first instanceof Promise) || scroller.scrollLeft !== 12 || scroller.scrollTop !== 15 ||
                window.scrollX !== 0 || window.scrollY !== 0)
              throw new Error('Element.scrollTo must update only its own scroll node');
            const second = scroller.scrollBy({top: 7});
            if (!(second instanceof Promise) || scroller.scrollLeft !== 12 || scroller.scrollTop !== 22)
              throw new Error('Element.scrollBy dictionary overload');
            scroller.scroll({left: 5});
            if (scroller.scrollLeft !== 5 || scroller.scrollTop !== 22)
              throw new Error('Element.scroll dictionary overload');
            scroller.scrollTo({behavior: 'instant', top: 4});
            scroller.scrollLeft === 5 && scroller.scrollTop === 4 && window.scrollX === 0 && window.scrollY === 0
            "#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
