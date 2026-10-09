use super::*;
use crate::realm_services::RealmServices;
use lumen::embed::Promise;
use lumen_bind::OneOrNumberPair;

pub(crate) fn window_open(ctx: &mut Ctx, source: &Rc<DomRealm>, url: &str, target: &str, features: &str) -> OpResult<Value> {
    let caller = ctx.invocation_host_realm();
    let entry = ctx.with_host_realm(&caller, |ctx|
        RealmServices::<WindowRealm>::current(ctx).and_then(|state| state.0.upgrade()))
        .map_err(browsing_context::host_realm_error)?;
    let source = entry.as_ref().unwrap_or(source);
    let Some(context)=source.browsing_context().filter(|context|context.is_active()) else{return Ok(Value::Null)};
    // HTML window-open steps parse before choosing or creating a target.
    let url = if url.is_empty() { None } else {
        Some(lumen_common::url::parse(url, Some(&source.base_url()))
            .map_err(|_| crate::error_reporting::dom_exception(ctx, "SyntaxError", "Invalid window URL"))?.href())
    };
    let features=features.to_ascii_lowercase();
    let disowned=features.split(|character:char|character==',' || character.is_ascii_whitespace()).any(|feature|matches!(feature.split('=').next(),Some("noopener"|"noreferrer")) && !matches!(feature.split('=').nth(1),Some("0"|"no")));
    let target=if target.is_empty(){"_blank"}else{target};
    let target=if disowned && !matches!(target.to_ascii_lowercase().as_str(),"_self"|"_parent"|"_top"){"_blank"}else{target};
    let Some(target)=context.choose_navigation_target(ctx,target)? else{return Ok(Value::Null)};
    if disowned {target.disown_opener();}
    if let Some(url) = url {
        let mut metadata=browsing_context::NavigationMetadata::from_document(source);
        if features.split(|character:char|character==',' || character.is_ascii_whitespace()).any(|feature|feature.split('=').next()==Some("noreferrer") && !matches!(feature.split('=').nth(1),Some("0"|"no"))) {metadata.referrer.policy=lumen_common::referrer::ReferrerPolicy::NoReferrer;}
        target.request_hyperlink_navigation(ctx,&url,source,metadata)?;
    }
    Ok(if disowned {Value::Null}else{target.proxy().unwrap_or(Value::Null)})
}

pub(crate) fn handler_get(ctx:&mut Ctx,receiver:&Value,event:&str,lenient:bool)->OpResult<Value> {
    let data=match ctx.with_instance::<DomWindow,_>(receiver,|window|window.base.data_handle()) {
        Ok(data)=>data,
        Err(_) if lenient=>return Ok(Value::Undefined),
        Err(error)=>return Err(error),
    };
    DomEventTarget::from_data(data).handler_value(ctx,receiver,event)
}

pub(crate) fn handler_set(ctx:&mut Ctx,receiver:&Value,event:&str,callback:events::EventHandler,lenient:bool)->OpResult<()> {
    let data=match ctx.with_instance::<DomWindow,_>(receiver,|window|window.base.data_handle()) {
        Ok(data)=>data,
        Err(_) if lenient=>return Ok(()),
        Err(error)=>return Err(error),
    };
    DomEventTarget::from_data(data).set_event_handler(ctx,receiver,event,callback);
    Ok(())
}

fn window_attribute_receiver(ctx: &mut Ctx, receiver: Value) -> Value {
    if matches!(receiver, Value::Undefined | Value::Null) {ctx.global_object()} else {receiver}
}

fn window_attribute_realm(ctx: &mut Ctx, receiver: Value) -> OpResult<Option<Rc<DomRealm>>> {
    let receiver=window_attribute_receiver(ctx,receiver);
    ctx.with_instance::<DomWindow,_>(&receiver,|window|window.base.associated_realm())
}

fn window_attribute_metadata(ctx: &mut Ctx, realm: &Option<Rc<DomRealm>>) -> Option<Rc<browsing_context::RealmMetadata>> {
    let handle=realm.as_ref()?.relevant_host_realm(ctx)?;
    browsing_context::metadata_for_realm(ctx,&handle)
}

fn replace_window_attribute(ctx: &mut Ctx, receiver: Value, name: &str, value: Value) -> OpResult<()> {
    // Web IDL's Replaceable setter accepts the original JS value, without
    // converting it to the getter's IDL type. Check brand/security before
    // creating the writable, enumerable, configurable own data property.
    let receiver = window_attribute_receiver(ctx, receiver);
    ctx.with_instance::<DomWindow, _>(&receiver, |_| ())?;
    ctx.create_data_property(&receiver, name, value).map_err(OpError::thrown)
}

fn scroll_window_receiver(
    ctx: &mut Ctx,
    receiver: &Value,
    args: OneOrNumberPair<scrolling::ScrollToOptions>,
    relative: bool,
) -> OpResult<Promise<()>> {
    // Web IDL's global-interface operations use their relevant global for
    // null/undefined this. Publish its WindowProxy before projecting the
    // native receiver so original-caller authorization still runs.
    let receiver = if matches!(receiver, Value::Undefined | Value::Null)
        || receiver.object_identity() == ctx.global_this_value().object_identity() {
        browsing_context::current_realm_metadata(ctx)
            .map(|metadata|browsing_context::window_self_from_metadata(ctx,&metadata))
            .unwrap_or_else(||ctx.global_this_value())
    } else { receiver.clone() };
    // The browser global is published through its WindowProxy. Generated
    // `&DomWindow` projection only recognizes a WindowProxy while an internal
    // property Get/Set scope is active; a method call occurs after that scope
    // has ended. Resolve the actual receiver through the checked native Window
    // path so its brand and original-caller security policy are preserved.
    let realm = ctx
        .with_instance::<DomWindow, _>(&receiver, |window| window.base.associated_realm())?
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

pub(crate) const NAVIGATOR_OWNER: &str = "#lumen_window\u{1}navigator";

struct WindowRealm(std::rc::Weak<DomRealm>);
struct NamedPropertiesRealm(Rc<RefCell<std::rc::Weak<DomRealm>>>);

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

crate::event_content_handlers::bind_window_handlers! {
    #[getter]
    fn name(&self) -> String {
        self.base.associated_realm().and_then(|realm| realm.browsing_context())
            .filter(|context| context.is_active()).map_or_else(String::new, |context| context.target_name())
    }

    #[setter]
    fn set_name(&self, value: String) {
        if let Some(context) = self.base.associated_realm().and_then(|realm| realm.browsing_context()) {
            context.set_target_name(value);
        }
    }

    #[getter]
    fn closed(&self) -> bool {
        self.base.associated_realm().and_then(|realm| realm.browsing_context())
            .is_none_or(|context| !context.is_active())
    }

    #[getter]
    fn opener(&self) -> Value {
        self.base.associated_realm().and_then(|realm|realm.browsing_context()).and_then(|context|context.opener_proxy()).unwrap_or(Value::Null)
    }

    #[setter]
    fn set_opener(&self,value:Value) {
        if matches!(value,Value::Null) {if let Some(context)=self.base.associated_realm().and_then(|realm|realm.browsing_context()){context.disown_opener();}}
    }

    #[method]
    fn open(ctx:&mut Ctx,this:lumen_bind::This<Value>,#[default(lumen_host::webidl::OptUsv::default())] url:lumen_host::webidl::OptUsv,target:lumen_bind::Passed<Value>,features:lumen_bind::Passed<Value>)->OpResult<Value> {
        let target=crate::option_factory::optional_string(ctx,target)?.unwrap_or_else(||"_blank".into());
        let features=crate::option_factory::optional_string(ctx,features)?.unwrap_or_default();
        let Some(source)=window_attribute_realm(ctx,this.0)? else{return Ok(Value::Null)};
        window_open(ctx,&source,&url.0.unwrap_or_default(),&target,&features)
    }

    #[method]
    fn close(&self,ctx:&mut Ctx) {
        if let Some(context)=self.base.associated_realm().and_then(|realm|realm.browsing_context()){context.close_auxiliary(ctx);}
    }

    #[method]
    fn focus(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let context = browsing_context::message_receiver_context(ctx, &this.0)
            .ok_or_else(|| OpError::type_error("Illegal Window receiver"))?;
        browsing_context::focus_window_context(ctx, &context, false)
    }

    #[method]
    fn blur(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
        let context = browsing_context::message_receiver_context(ctx, &this.0)
            .ok_or_else(|| OpError::type_error("Illegal Window receiver"))?;
        browsing_context::focus_window_context(ctx, &context, true)
    }

    #[method(name = "postMessage")]
    fn post_message(
        ctx: &mut Ctx,
        this: lumen_bind::This<Value>,
        message: Value,
        #[default(window_messaging::PostMessageTarget::default())]
        target: window_messaging::PostMessageTarget,
        #[default(Value::Undefined)] transfer: Value,
    ) -> OpResult<()> {
        window_messaging::post_message(ctx, &this.0, message, target, transfer)
    }





    #[getter]
    fn history(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let realm = self.base.associated_realm()
            .ok_or_else(|| crate::error_reporting::dom_exception(ctx, "SecurityError", "Window document is unavailable"))?;
        history::value(ctx, &realm)
    }

    #[setter(name = "innerWidth")]
    fn set_inner_width(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "innerWidth", value)
    }

    #[setter(name = "innerHeight")]
    fn set_inner_height(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "innerHeight", value)
    }

    #[setter(name = "devicePixelRatio")]
    fn set_device_pixel_ratio(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "devicePixelRatio", value)
    }

    #[setter(name = "scrollX")]
    fn set_scroll_x(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "scrollX", value)
    }

    #[setter(name = "scrollY")]
    fn set_scroll_y(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "scrollY", value)
    }

    #[setter(name = "pageXOffset")]
    fn set_page_x_offset(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "pageXOffset", value)
    }

    #[setter(name = "pageYOffset")]
    fn set_page_y_offset(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "pageYOffset", value)
    }

    #[getter(hint(js(global)))]
    fn inner_width(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<u32> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.viewport_size().map(|size|size.0))?
    }

    #[getter(hint(js(global)))]
    fn device_pixel_ratio(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<f64> {
        let receiver = window_attribute_receiver(ctx, this.0);
        ctx.with_instance::<DomWindow, _>(&receiver, |window| {
            window.base.associated_realm().map(|realm| realm.device_pixel_ratio.get())
                .ok_or_else(|| OpError::new("SecurityError", "Window document is unavailable"))
        })?
    }

    #[getter(hint(js(global)))]
    fn inner_height(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<u32> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.viewport_size().map(|size|size.1))?
    }

    #[getter(hint(js(global)))]
    fn scroll_x(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<f64> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.scroll_position().map(|position|position.0))?
    }

    #[getter(hint(js(global)))]
    fn scroll_y(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<f64> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.scroll_position().map(|position|position.1))?
    }

    #[getter(hint(js(global)))]
    fn page_x_offset(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<f64> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.scroll_position().map(|position|position.0))?
    }

    #[getter(hint(js(global)))]
    fn page_y_offset(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<f64> {
        let receiver=window_attribute_receiver(ctx,this.0);
        ctx.with_instance::<DomWindow,_>(&receiver,|window|window.scroll_position().map(|position|position.1))?
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

    #[getter(hint(js(global)))]
    fn origin(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<String> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(realm.and_then(|realm|realm.document_origin()).map_or_else(||"null".into(),|origin|origin.serialize()))
    }

    #[setter(name = "origin")]
    fn set_origin(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "origin", value)
    }

    #[getter(name = "clientInformation", hint(js(global)))]
    fn client_information(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(realm.and_then(|realm|realm.relevant_host_realm(ctx))
            .and_then(|handle|ctx.native_private_value_slot(&handle.global(),NAVIGATOR_OWNER)).unwrap_or(Value::Undefined))
    }

    #[setter(name = "clientInformation")]
    fn set_client_information(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "clientInformation", value)
    }

    #[getter(name = "self", hint(js(global)))]
    fn window_self(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(window_attribute_metadata(ctx,&realm).map(|metadata|browsing_context::window_self_from_metadata(ctx,&metadata)).unwrap_or_else(||ctx.global_this_value()))
    }

    #[setter(name = "self")]
    fn set_window_self(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "self", value)
    }

    #[setter(name = "parent")]
    fn set_parent(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "parent", value)
    }

    #[setter(name = "frames")]
    fn set_frames(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "frames", value)
    }

    #[setter(name = "length")]
    fn set_length(ctx: &mut Ctx, this: lumen_bind::This<Value>, #[default(Value::Undefined)] value: Value) -> OpResult<()> {
        replace_window_attribute(ctx, this.0, "length", value)
    }

    #[getter(hint(js(global)))]
    fn parent(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(match realm.as_ref().and_then(|realm|realm.browsing_context()) {
            Some(context)=>browsing_context::window_parent_value(ctx,&context),
            None=>window_attribute_metadata(ctx,&realm).map(|metadata|browsing_context::window_parent_from_metadata(ctx,&metadata)).unwrap_or_else(||ctx.global_this_value()),
        })
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

    #[getter(hint(js(global)))]
    fn frames(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(match realm.as_ref().and_then(|realm|realm.browsing_context()) {
            Some(context)=>browsing_context::window_frames_value(ctx,&context),
            None=>window_attribute_metadata(ctx,&realm).map(|metadata|browsing_context::window_self_from_metadata(ctx,&metadata)).unwrap_or_else(||ctx.global_this_value()),
        })
    }

    #[getter(hint(js(global)))]
    fn length(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<u32> {
        let realm=window_attribute_realm(ctx,this.0)?;
        Ok(realm.and_then(|realm|realm.browsing_context()).map_or(0,|context|browsing_context::window_length(&context)))
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
    fn set_location(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        let context = browsing_context::current_realm_context(ctx)
            .ok_or_else(|| OpError::new("InvalidStateError", "Window has no active context"))?;
        let entry_base = entry_base_url(ctx, &context);
        context.request_location_navigation_with_caller(ctx, &value.0, &entry_base)
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











}

impl lumen::embed::NativeIdentityOwner for DomWindow {
    const TRACES_NATIVE_VALUES: bool = true;

    fn trace_native_identities(&self, _: u64, _: &mut dyn FnMut(&Value)) {}

    fn trace_native_values(&self, visit: &mut dyn FnMut(&Value)) {
        self.base.trace_callback_values(visit);
        if let Some(realm) = self.base.associated_realm() {
            if let Some(data) = realm.history_data.borrow().as_ref() { data.trace_values(visit); }
            if let Some(data) = realm.fragment_state.borrow().as_ref() { data.trace_values(visit); }
            if let Some(value) = realm.history_wrapper.borrow().as_ref().and_then(WeakValue::upgrade) { visit(&value); }
        }
    }
}

#[lumen_bind::class(name = "Location", hint(js(webidl)))]
pub(crate) struct DomLocation {
    context: std::rc::Weak<browsing_context::BrowsingContext>,
    owner: RefCell<std::rc::Weak<DomRealm>>,
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
            .borrow()
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

    fn url(&self, ctx: &mut Ctx) -> OpResult<lumen_common::url::Url> {
        let (owner, context) = self.active_owner_and_context()?;
        browsing_context::require_same_origin_context(ctx, &context)?;
        let url = owner
            .document_url()
            .unwrap_or_else(|| "about:blank".to_owned());
        lumen_common::url::parse(&url, None)
            .map_err(|_| OpError::new("InvalidStateError", "document URL is invalid"))
    }

    fn navigate(&self, ctx: &mut Ctx, input: &str) -> OpResult<()> {
        let (_, context) = self.active_owner_and_context()?;
        let entry_base = entry_base_url(ctx, &context);
        context.request_location_navigation_with_caller(ctx, input, &entry_base)
    }

    fn update(
        &self,
        ctx: &mut Ctx,
        change: impl FnOnce(&mut lumen_common::url::Url) -> bool,
    ) -> OpResult<()> {
        let mut url = self.url(ctx)?;
        if change(&mut url) {
            self.navigate(ctx, &url.href())?;
        }
        Ok(())
    }
}

#[lumen_bind::methods]
impl DomLocation {
    #[getter]
    fn href(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self.url(ctx)?.href())
    }

    #[setter(coerce)]
    fn set_href(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.navigate(ctx, &value.0)
    }

    #[getter]
    fn origin(&self, ctx: &mut Ctx) -> OpResult<String> {
        let (_, context) = self.active_owner_and_context()?;
        browsing_context::require_same_origin_context(ctx, &context)?;
        Ok(browsing_context::context_origin(&context).serialize())
    }

    #[getter]
    fn protocol(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(format!("{}:", self.url(ctx)?.scheme))
    }

    #[setter(coerce)]
    fn set_protocol(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| url.set_protocol(&value.0))
    }

    #[getter]
    fn host(&self, ctx: &mut Ctx) -> OpResult<String> {
        let url = self.url(ctx)?;
        let mut host = url.host.unwrap_or_default();
        if let Some(port) = url.port {
            host.push(':');
            host.push_str(&port.to_string());
        }
        Ok(host)
    }

    #[setter(coerce)]
    fn set_host(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| url.set_host(&value.0))
    }

    #[getter]
    fn hostname(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self.url(ctx)?.host.unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_hostname(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| url.set_hostname(&value.0))
    }

    #[getter]
    fn port(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self
            .url(ctx)?
            .port
            .map(|port| port.to_string())
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_port(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| url.set_port(&value.0))
    }

    #[getter]
    fn pathname(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self.url(ctx)?.path)
    }

    #[setter(coerce)]
    fn set_pathname(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| url.set_pathname(&value.0))
    }

    #[getter]
    fn search(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self
            .url(ctx)?
            .query
            .filter(|query| !query.is_empty())
            .map(|query| format!("?{query}"))
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_search(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| {
            url.set_search(&value.0);
            true
        })
    }

    #[getter]
    fn hash(&self, ctx: &mut Ctx) -> OpResult<String> {
        Ok(self
            .url(ctx)?
            .fragment
            .filter(|fragment| !fragment.is_empty())
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_hash(&self, ctx: &mut Ctx, value: lumen_host::webidl::Usv) -> OpResult<()> {
        self.update(ctx, |url| {
            // Location's compatibility bailout compares a null old fragment
            // as the empty string, while the new URL record retains a real
            // empty fragment if a navigation is required (HTML hash steps 4–8).
            let previous = url.fragment.clone().unwrap_or_default();
            url.set_hash(&value.0);
            if url.fragment.is_none() { url.fragment = Some(String::new()); }
            url.fragment.as_deref() != Some(previous.as_str())
        })
    }

    #[method]
    fn assign(&self, ctx: &mut Ctx, url: lumen_host::webidl::Usv) -> OpResult<()> {
        let (_, context) = self.active_owner_and_context()?;
        browsing_context::require_same_origin_context(ctx, &context)?;
        self.navigate(ctx, &url.0)
    }

    #[method]
    fn replace(&self, ctx: &mut Ctx, url: lumen_host::webidl::Usv) -> OpResult<()> {
        let (_, context) = self.active_owner_and_context()?;
        let entry_base = entry_base_url(ctx, &context);
        context.request_location_navigation_with_handling(ctx, &url.0, &entry_base, true)
    }

    #[method]
    fn reload(&self, ctx: &mut Ctx) -> OpResult<()> {
        self.url(ctx)?;
        let (_, context) = self.active_owner_and_context()?;
        super::history::reload(ctx, &context)
    }

    #[method(name = "toString")]
    fn to_string(&self, ctx: &mut Ctx) -> OpResult<String> {
        self.href(ctx)
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
        owner: RefCell::new(Rc::downgrade(realm)),
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
    realm: Rc<RefCell<std::rc::Weak<DomRealm>>>,
    global: WeakValue,
}

impl WindowNamedPropertiesHandler {
    fn supported_names(&self, ctx: &mut Ctx) -> OpResult<Vec<String>> {
        let Some(realm) = self.realm.borrow().upgrade() else { return Ok(Vec::new()); };
        let windows = realm.named_child_windows(ctx)?.into_iter()
            .map(|(node, name, _)| (node, name)).collect::<HashMap<_, _>>();
        let session = realm.session.borrow();
        let document = session.document();
        let root = document.root();
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        let mut next = super::next_descendant(document, root, root).map_err(super::dom_error)?;
        while let Some(node) = next {
            if let Some(name) = windows.get(&node) {
                if seen.insert(name.clone()) { names.push(name.clone()); }
            }
            if let Ok(NodeKind::Element { namespace, name, .. }) = document.kind(node) {
                if matches!(namespace, Namespace::Html) && ["embed", "form", "img", "object"]
                    .iter().any(|tag| lumen_html::svg::local_name(name).eq_ignore_ascii_case(tag))
                {
                    if let Some(name) = document.get_attribute_ns_ref(node, None, "name").map_err(super::dom_error)? {
                        if !name.is_empty() && seen.insert(name.to_owned()) { names.push(name.to_owned()); }
                    }
                }
                if let Some(name) = document.get_attribute_ns_ref(node, None, "id").map_err(super::dom_error)? {
                    if !name.is_empty() && seen.insert(name.to_owned()) { names.push(name.to_owned()); }
                }
            }
            next = super::next_descendant(document, root, node).map_err(super::dom_error)?;
        }
        Ok(names)
    }

    fn named_nodes(&self, name: &str) -> OpResult<Vec<NodeId>> {
        let Some(realm) = self.realm.borrow().upgrade() else {
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
        let navigable = match self.realm.borrow().upgrade() {
            Some(realm) => realm.named_child_windows(ctx)?.iter().any(|(_, candidate, _)| candidate == name),
            None => false,
        };
        if !navigable && self.named_nodes(name)?.is_empty() {
            return Ok(false);
        }
        let inherited = ctx
            .reflect_has(target, &Value::str(name))
            .map_err(OpError::thrown)?;
        Ok(!inherited)
    }

    fn named_value(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        let Some(realm) = self.realm.borrow().upgrade() else {
            return Ok(Value::Undefined);
        };
        if let Some(proxy) = realm.named_child_value(ctx, name)?
        { return Ok(proxy); }
        let nodes = self.named_nodes(name)?;
        match nodes.as_slice() {
            [] => Ok(Value::Undefined),
            [node] => Ok(realm.wrap(ctx, *node)),
            _ => {
                let root = realm.session.borrow().document().root();
                let owner = self.global.upgrade().unwrap_or(Value::Undefined);
                Ok(DomHtmlCollection::create(ctx, DomNodeList::descendants(
                        realm,
                        root,
                        DescendantFilter::WindowNamed(name.to_owned()),
                        owner,
                    )))
            }
        }
    }
}

#[lumen_bind::methods]
impl WindowNamedPropertiesHandler {
    #[method(name = "ownKeys")]
    fn own_keys(&self, ctx: &mut Ctx, target: Value) -> OpResult<Vec<Value>> {
        let mut keys = Vec::new();
        for name in self.supported_names(ctx)? {
            if self.named_property_visible(ctx, &target, &name)? { keys.push(Value::str(name)); }
        }
        for key in ctx.reflect_own_keys(&target).map_err(OpError::thrown)? {
            if !matches!(&key, Value::Str(name) if keys.iter().any(|existing| matches!(existing, Value::Str(existing) if existing.as_str() == name.as_str()))) {
                keys.push(key);
            }
        }
        Ok(keys)
    }

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
            get_prototype_of.clone(),
            object.clone(),
            std::slice::from_ref(&global),
        )
        .map_err(OpError::thrown)?;
    let inherited_prototype = ctx.invoke(get_prototype_of.clone(), object.clone(),
        std::slice::from_ref(&previous_prototype)).map_err(OpError::thrown)?;
    let target = ctx.new_object_with_proto(&inherited_prototype);
    let owner = Rc::new(RefCell::new(Rc::downgrade(realm)));
    RealmServices::replace_current(ctx, NamedPropertiesRealm(owner.clone()));
    let handler = ctx.new_instance(WindowNamedPropertiesHandler {
        realm: owner,
        global: ctx.weak_value(&global).expect("window global is an object"),
    });
    let named_properties = ctx.create_proxy(target, handler).map_err(OpError::thrown)?;
    ctx.invoke(set_prototype_of, object, &[previous_prototype, named_properties])
        .map_err(OpError::thrown)?;
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    RealmServices::replace_current(ctx, WindowRealm(Rc::downgrade(realm)));
    lumen_host::net::set_api_base_url_provider(ctx, |ctx|
        current_dom_realm(ctx).map(|realm|realm.base_url()));
    window_messaging::install(ctx)?;
    let function = ctx.bound_function(&lumen_bind::FnItem::of::<get_selection::Op>());
    let global = ctx.global_object();
    let constructor=ctx.class_constructor::<DomWindow>();
    let prototype=ctx.member_get(&constructor,"prototype").map_err(OpError::thrown)?;
    let mut initial_handlers=Vec::new();
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
            initial_handlers.push((name,callback));
        }
    }
    // Web IDL global-interface attributes are own, configurable properties.
    // Reuse the typed prototype's canonical accessor functions and descriptors.
    for &name in event_content_handlers::WINDOW_IDL_HANDLERS {
        let key=Value::str(name);
        let descriptor=ctx.reflect_get_own_property_descriptor(&prototype,&key).map_err(OpError::thrown)?;
        if !ctx.reflect_define_property(&global,&key,&descriptor).map_err(OpError::thrown)? {
            return Err(OpError::type_error("Window handler attribute could not be installed"));
        }
    }
    for (name,callback) in initial_handlers {
        let callback=events::EventHandler((matches!(callback,Value::Obj(_))||callback.is_callable()).then_some(callback));
        handler_set(ctx,&global,name.trim_start_matches("on"),callback,false)?;
    }
    ctx.set_member(&global, "getSelection", function)
        .map_err(|_| OpError::new("Error", "getSelection install failed"))?;
    ctx.install_global_attributes::<DomWindow>().map_err(OpError::thrown)?;
    install_named_properties(ctx, realm)
}

pub(crate) fn rebind_document(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let global = ctx.global_object();
    if let Some(old) = current_dom_realm(ctx) {
        if let Some(location) = old.location_wrapper.borrow().as_ref().and_then(WeakValue::upgrade) {
            ctx.with_instance::<DomLocation, _>(&location, |location| {
                *location.owner.borrow_mut() = Rc::downgrade(realm);
            })?;
            *realm.location_wrapper.borrow_mut() = ctx.weak_value(&location);
        }
    }
    let target = ctx.with_instance::<DomWindow, _>(&global, |window| window.base.data_handle())?;
    let window = DomEventTarget::from_data(target.clone());
    window.rebind_window(realm);
    *realm.window_target.borrow_mut() = Some(target);
    RealmServices::replace_current(ctx, WindowRealm(Rc::downgrade(realm)));
    if let Some(owner) = RealmServices::<NamedPropertiesRealm>::current(ctx) {
        *owner.0.borrow_mut() = Rc::downgrade(realm);
    }
    Ok(())
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
    fn specification_window_replaceable_attributes_have_own_data_replacements() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = script(&mut engine, r#"
            (() => {
                'use strict';
                const w = window;
                const names = ['self', 'frames', 'length', 'parent', 'origin', 'clientInformation',
                    'innerWidth', 'innerHeight', 'scrollX', 'scrollY', 'pageXOffset', 'pageYOffset'];
                if (w.self !== w || w.frames !== w || w.parent !== w ||
                    w.clientInformation !== w.navigator || w.length !== 0 || w.origin !== 'null') return false;
                for (const name of names) {
                    const descriptor = Object.getOwnPropertyDescriptor(w, name);
                    if (typeof descriptor.get !== 'function' || typeof descriptor.set !== 'function' ||
                        descriptor.get.length !== 0 || descriptor.set.length !== 1 ||
                        !descriptor.enumerable || !descriptor.configurable ||
                        Object.prototype.hasOwnProperty.call(Window.prototype, name)) return false;
                    const unbranded = {};
                    let branded = false;
                    try { descriptor.set.call(unbranded, 1); } catch (e) { branded = e instanceof TypeError; }
                    if (!branded || Object.prototype.hasOwnProperty.call(unbranded, name)) return false;
                    const replacement = {valueOf() { throw new Error('must not convert'); }};
                    w[name] = replacement;
                    const data = Object.getOwnPropertyDescriptor(w, name);
                    if (w[name] !== replacement || data.value !== replacement ||
                        !data.writable || !data.enumerable || !data.configurable || data.get !== undefined) return false;
                    Object.defineProperty(w, name, {value: 7, writable: false, configurable: true});
                    let strictRejected = false;
                    try { w[name] = 9; } catch (e) { strictRejected = e instanceof TypeError; }
                    if (!strictRejected || w[name] !== 7 || !delete w[name] || name in w) return false;
                    descriptor.set.call(w);
                    if (!Object.prototype.hasOwnProperty.call(w, name) || w[name] !== undefined) return false;
                    delete w[name];
                    descriptor.set.call(null, replacement);
                    if (w[name] !== replacement) return false;
                    delete w[name];
                    Object.defineProperty(w, name, descriptor);
                }
                const clientGetter = Object.getOwnPropertyDescriptor(w, 'clientInformation').get;
                const originalNavigator = w.navigator;
                w.navigator = {shadow: true};
                const stable = clientGetter.call(w) === originalNavigator;
                w.navigator = originalNavigator;
                return stable;
            })()
        "#);
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn specification_window_replaceable_reinstall_preserves_authored_own_properties() {
        let mut engine = Engine::new();
        let original = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(script(&mut engine, "window.frames=42;delete window.origin;Object.defineProperty(window,'parent',{value:19,writable:false,configurable:false});frames===42&&parent===19&&!('origin' in window)"),Value::Bool(true)));
        engine.ctx().install_global_attributes::<DomWindow>().ok().expect("typed global attribute reinstall");
        let successor = crate::install(engine.ctx(), "<section></section>", 64).unwrap();
        assert!(!Rc::ptr_eq(&original,&successor));
        assert!(matches!(script(&mut engine, "frames===42&&parent===19&&!('origin' in window)&&Object.getOwnPropertyDescriptor(window,'parent').configurable===false"),Value::Bool(true)));
    }

    #[test]
    fn specification_window_replaceable_window_proxy_preserves_foreign_safe_attributes() {
        let mut engine = Engine::new();
        let parent = crate::install(engine.ctx(), "<main></main>", 64).unwrap();
        parent.set_document_url("https://parent.example.test/index.html");
        script(&mut engine, "const pendingChild = document.createElement('iframe'); pendingChild.id = 'child'; document.body.appendChild(pendingChild);");
        let node = { let session = parent.session.borrow();
            lumen_html::selector::get_element_by_id(session.document(), session.document().root(), "child")
                .unwrap().unwrap() };
        let frame = parent.ensure_frame_context(engine.ctx(), node).unwrap();
        let child = frame.current_document().unwrap();
        assert!(matches!(script(&mut engine, r#"
            globalThis.replaceableChild = document.getElementById('child').contentWindow;
            globalThis.replaceableSetter = Object.getOwnPropertyDescriptor(window, 'frames').set;
            const childNavigator = replaceableChild.navigator;
            const childInfoGetter = Object.getOwnPropertyDescriptor(window, 'clientInformation').get;
            const borrowedInfo = childInfoGetter.call(replaceableChild);
            const borrowedParent = Object.getOwnPropertyDescriptor(window, 'parent').get.call(replaceableChild);
            const borrowedSelf = Object.getOwnPropertyDescriptor(window, 'self').get.call(replaceableChild);
            const borrowedFrames = Object.getOwnPropertyDescriptor(window, 'frames').get.call(replaceableChild);
            const borrowedLength = Object.getOwnPropertyDescriptor(window, 'length').get.call(replaceableChild);
            if (borrowedInfo !== childNavigator || borrowedInfo === navigator || borrowedParent !== window ||
                borrowedSelf !== replaceableChild || borrowedFrames !== replaceableChild || borrowedLength !== 0)
                throw Error('borrowed getter used its creation global instead of its receiver');
            const marker = {};
            replaceableSetter.call(replaceableChild, marker);
            replaceableChild.frames === marker && window.frames === window &&
                Object.getOwnPropertyDescriptor(replaceableChild, 'frames').writable
        "#), Value::Bool(true)));
        child.set_document_url("https://foreign.example.test/child.html");
        assert!(matches!(script(&mut engine, r#"
            (() => {
                'use strict';
                const foreign = replaceableChild;
                const descriptor = Object.getOwnPropertyDescriptor(foreign, 'frames');
                let assignment = false, borrowed = false, redefine = false;
                try { foreign.frames = {}; } catch (e) { assignment = e.name === 'SecurityError'; }
                try { replaceableSetter.call(foreign, {}); } catch (e) { borrowed = e.name === 'SecurityError'; }
                try { Object.defineProperty(foreign, 'frames', {value: 1}); }
                catch (e) { redefine = e.name === 'SecurityError'; }
                return assignment && borrowed && redefine && foreign.frames === foreign &&
                    foreign.self === foreign && foreign.parent === window && foreign.length === 0 &&
                    typeof descriptor.get === 'function' && descriptor.set === undefined &&
                    descriptor.configurable && !descriptor.enumerable;
            })()
        "#), Value::Bool(true)));
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
            const namedPrototype = Object.getPrototypeOf(Object.getPrototypeOf(window));
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
