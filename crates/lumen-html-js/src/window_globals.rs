use super::*;
use crate::realm_services::RealmServices;

struct WindowRealm(std::rc::Weak<DomRealm>);

#[lumen_bind::class(name = "Window", extends = DomEventTarget, hint(js(webidl)))]
pub(crate) struct DomWindow {
    base: DomEventTarget,
}

impl DomWindow {
    pub(crate) fn from_target(base: DomEventTarget) -> Self {
        Self { base }
    }
}

#[lumen_bind::methods]
impl DomWindow {
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
    fn onunhandledrejection(&self) -> Option<lumen::embed::JsFunction> {
        self.base.handler("unhandledrejection")
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
    fn onrejectionhandled(&self) -> Option<lumen::embed::JsFunction> {
        self.base.handler("rejectionhandled")
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
}
