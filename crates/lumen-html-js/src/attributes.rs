//! Native `Attr` and live `NamedNodeMap` views over the shared HTML document.
//!
//! Attribute nodes are materialized lazily by `lumen-html::Document`; this adapter keeps their
//! canonical JavaScript identity through the normal per-NodeId wrapper cache. `NamedNodeMap` is
//! a native indexed object rather than an array snapshot, so its length and indexed entries read
//! directly from the live Element each time.

use super::{dom_error, error_reporting, event_content_handlers, forms, DomNode, DomRealm};
use lumen::embed::{Ctx, Nullable, OpError, OpResult, Value, WeakValue};
use lumen_html::{Error, Namespace, NodeId, NodeKind};
use std::rc::Rc;

#[lumen_bind::class(name = "Attr", extends = DomNode, hint(js(webidl)))]
pub(crate) struct DomAttr {
    pub(crate) base: DomNode,
}

#[lumen_bind::methods]
impl DomAttr {
    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self.attribute()?.1)
    }

    #[getter]
    fn local_name(&self) -> OpResult<String> {
        let (namespace, name, _) = self.attribute()?;
        Ok(if namespace.is_some() {
            name.rsplit_once(':')
                .map_or(name.as_str(), |(_, local)| local)
                .to_owned()
        } else {
            name
        })
    }

    #[getter(name = "namespaceURI")]
    fn namespace_uri(&self) -> OpResult<Nullable<String>> {
        Ok(Nullable(self.attribute()?.0))
    }

    #[getter]
    fn prefix(&self) -> OpResult<Nullable<String>> {
        let (namespace, name, _) = self.attribute()?;
        Ok(Nullable(namespace.and_then(|_| name.split_once(':').map(|(prefix, _)| prefix.to_owned()))))
    }

    #[getter]
    fn value(&self) -> OpResult<String> {
        Ok(self.attribute()?.2)
    }

    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_value(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_attr_value(ctx, &self.base.realm, self.base.id, value)
    }

    #[getter]
    fn specified(&self) -> bool {
        true
    }

    #[getter(name = "ownerElement")]
    fn owner_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let owner = match self
            .base
            .realm
            .session
            .borrow()
            .document()
            .kind(self.base.id)
            .map_err(dom_error)?
        {
            NodeKind::Attribute { owner_element, .. } => owner_element.as_deref().copied(),
            _ => {
                return Err(OpError::new(
                    "InvalidStateError",
                    "Attr node is no longer available",
                ));
            }
        };
        Ok(owner.map_or(Value::Null, |owner| self.base.realm.wrap(ctx, owner)))
    }

    // Node's inherited data accessors are overridden because Attr has scalar text data rather
    // than children. Keep all three standard views synchronized through the same core record.
    #[getter(name = "nodeValue")]
    fn node_value(&self) -> OpResult<String> {
        self.value()
    }

    #[setter(name = "nodeValue", coerce, hint(js(ce_reactions)))]
    fn set_node_value(&self, ctx: &mut Ctx, value: Option<&str>) -> OpResult<()> {
        set_attr_value(ctx, &self.base.realm, self.base.id, value.unwrap_or(""))
    }

    #[getter(name = "textContent")]
    fn text_content(&self) -> OpResult<String> {
        self.value()
    }

    #[setter(name = "textContent", coerce, hint(js(ce_reactions)))]
    fn set_text_content(&self, ctx: &mut Ctx, value: &str) -> OpResult<()> {
        set_attr_value(ctx, &self.base.realm, self.base.id, value)
    }
}

impl DomAttr {
    fn attribute(&self) -> OpResult<(Option<String>, String, String)> {
        let session = self.base.realm.session.borrow();
        match session.document().kind(self.base.id).map_err(dom_error)? {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                value,
                ..
            } => Ok((
                namespace_uri
                    .as_ref()
                    .map(|namespace| namespace.to_string()),
                qualified_name.as_str().to_owned(),
                value.clone(),
            )),
            _ => Err(OpError::new(
                "InvalidStateError",
                "Attr node is no longer available",
            )),
        }
    }
}

#[lumen_bind::class(name = "NamedNodeMap", hint(js(webidl, named_properties)))]
pub(crate) struct DomNamedNodeMap {
    realm: Rc<DomRealm>,
    element: NodeId,
    _owner: Value,
}

#[lumen_bind::methods]
impl DomNamedNodeMap {
    // These three methods are private hooks consumed by Lumen's trusted native indexed-object
    // facade. Their `js(...)` hints prevent them from appearing on NamedNodeMap.prototype. The
    // name enumerator runs only when ownKeys is requested; the single-name getter materializes
    // only the Attr that is actually read.
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> OpResult<bool> {
        self.realm
            .session
            .borrow()
            .document()
            .supports_attribute_name(self.element, name)
            .map_err(dom_error)
    }

    #[method(hint(js(named_names)))]
    fn named_names(&self) -> OpResult<Vec<String>> {
        self.realm
            .session
            .borrow()
            .document()
            .supported_attribute_names(self.element)
            .map_err(dom_error)
    }

    #[method(hint(js(named_getter)))]
    fn named_getter(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        self.get_named_item(ctx, name)
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        self.realm
            .session
            .borrow()
            .document()
            .attribute_count(self.element)
            .map_err(dom_error)
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_at(self.element, index)
            .map_err(dom_error)?;
        Ok(attribute.map_or(Value::Undefined, |attribute| {
            self.realm.wrap(ctx, attribute)
        }))
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_at(self.element, index)
            .map_err(dom_error)?;
        Ok(attribute.map_or(Value::Null, |attribute| self.realm.wrap(ctx, attribute)))
    }

    #[method(name = "getNamedItem", coerce)]
    fn get_named_item(&self, ctx: &mut Ctx, qualified_name: &str) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_by_name(self.element, qualified_name)
            .map_err(dom_error)?;
        Ok(attribute.map_or(Value::Null, |attribute| self.realm.wrap(ctx, attribute)))
    }

    #[method(name = "getNamedItemNS", coerce)]
    fn get_named_item_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_by_ns(self.element, namespace_uri, local_name)
            .map_err(dom_error)?;
        Ok(attribute.map_or(Value::Null, |attribute| self.realm.wrap(ctx, attribute)))
    }

    #[method(name = "setNamedItem", hint(js(ce_reactions)))]
    fn set_named_item(&self, ctx: &mut Ctx, attribute: &DomAttr) -> OpResult<Value> {
        self.set_attribute_node(ctx, attribute, false)
    }

    #[method(name = "setNamedItemNS", hint(js(ce_reactions)))]
    fn set_named_item_ns(&self, ctx: &mut Ctx, attribute: &DomAttr) -> OpResult<Value> {
        self.set_attribute_node(ctx, attribute, true)
    }

    #[method(name = "removeNamedItem", coerce, hint(js(ce_reactions)))]
    fn remove_named_item(&self, ctx: &mut Ctx, qualified_name: &str) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_by_name(self.element, qualified_name)
            .map_err(dom_error)?
            .ok_or_else(|| missing_attribute(ctx, qualified_name))?;
        self.remove_attribute_node(ctx, attribute)
    }

    #[method(name = "removeNamedItemNS", coerce, hint(js(ce_reactions)))]
    fn remove_named_item_ns(
        &self,
        ctx: &mut Ctx,
        namespace_uri: Option<&str>,
        local_name: &str,
    ) -> OpResult<Value> {
        let attribute = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .attribute_node_by_ns(self.element, namespace_uri, local_name)
            .map_err(dom_error)?
            .ok_or_else(|| missing_attribute(ctx, local_name))?;
        self.remove_attribute_node(ctx, attribute)
    }
}

impl DomNamedNodeMap {
    pub(crate) fn adopt_node(&mut self, realm: Rc<DomRealm>, element: NodeId) {
        self.realm = realm;
        self.element = element;
    }

    fn set_attribute_node(
        &self,
        ctx: &mut Ctx,
        attribute: &DomAttr,
        namespace_aware: bool,
    ) -> OpResult<Value> {
        let source_realm = attribute.base.realm.clone();
        let source_node = attribute.base.id;
        let (owner, namespace_uri, qualified_name) = match source_realm
            .session
            .borrow()
            .document()
            .kind(source_node)
            .map_err(dom_error)?
        {
            NodeKind::Attribute {
                owner_element,
                namespace_uri,
                qualified_name,
                ..
            } => (
                owner_element.as_deref().copied(),
                namespace_uri.as_deref().map(|uri| uri.as_ref().to_owned()),
                qualified_name.as_str().to_owned(),
            ),
            _ => return Err(OpError::type_error("setNamedItem requires an Attr node")),
        };
        if owner.is_some() && !Rc::ptr_eq(&source_realm, &self.realm) {
            return Err(in_use_attribute(ctx));
        }
        if namespace_uri.is_none() {
            forms::prepare_input_attribute_change(&self.realm, self.element, &qualified_name)?;
        }
        let attribute_id = if Rc::ptr_eq(&source_realm, &self.realm) {
            source_node
        } else {
            DomRealm::adopt_node_from(&self.realm, ctx, &source_realm, source_node)?
        };
        let replaced = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_node(self.element, attribute_id, namespace_aware)
            .map_err(|error| attribute_error(ctx, error))?;
        self.attribute_changed(ctx, attribute_id, true)?;
        Ok(replaced.map_or(Value::Null, |node| self.realm.wrap(ctx, node)))
    }

    fn remove_attribute_node(&self, ctx: &mut Ctx, attribute: NodeId) -> OpResult<Value> {
        let (namespace_uri, qualified_name) = match self
            .realm
            .session
            .borrow()
            .document()
            .kind(attribute)
            .map_err(dom_error)?
        {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                ..
            } => (
                namespace_uri.as_deref().map(|uri| uri.as_ref().to_owned()),
                qualified_name.as_str().to_owned(),
            ),
            _ => {
                return Err(OpError::new(
                    "InvalidStateError",
                    "Attr node is no longer available",
                ));
            }
        };
        if namespace_uri.is_none() {
            forms::prepare_input_attribute_change(&self.realm, self.element, &qualified_name)?;
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute_node(self.element, attribute)
            .map_err(|error| attribute_error(ctx, error))?;
        let value = self.realm.wrap(ctx, attribute);
        self.attribute_changed(ctx, attribute, false)?;
        self.realm.reap_detached([attribute]);
        Ok(value)
    }

    fn attribute_changed(&self, ctx: &mut Ctx, attribute: NodeId, attached: bool) -> OpResult<()> {
        let (namespace_uri, qualified_name, value) = match self
            .realm
            .session
            .borrow()
            .document()
            .kind(attribute)
            .map_err(dom_error)?
        {
            NodeKind::Attribute {
                namespace_uri,
                qualified_name,
                value,
                ..
            } => (
                namespace_uri.as_deref().map(|uri| uri.as_ref().to_owned()),
                qualified_name.as_str().to_owned(),
                attached.then(|| value.clone()),
            ),
            _ => {
                return Err(OpError::new(
                    "InvalidStateError",
                    "Attr node is no longer available",
                ));
            }
        };
        if namespace_uri.is_none() {
            forms::resanitize_input_after_attribute_change(
                &self.realm,
                self.element,
                &qualified_name,
            )?;
        }
        self.realm.sync_image_bitmaps()?;
        event_content_handlers::attribute_changed(
            ctx,
            &self.realm,
            self.element,
            namespace_uri.as_deref(),
            &qualified_name,
            value.as_deref(),
        )?;
        self.realm.flush_script_activations(ctx)
    }
}

pub(crate) fn named_map(ctx: &mut Ctx, element: &DomNode, owner: Value) -> OpResult<Value> {
    if let Some(value) = element
        .collections
        .borrow()
        .get("attributes")
        .and_then(WeakValue::upgrade)
    {
        return Ok(value);
    }
    let value = ctx.new_instance(DomNamedNodeMap {
        realm: element.realm.clone(),
        element: element.id,
        _owner: owner,
    });
    element.collections.borrow_mut().insert(
        "attributes".into(),
        ctx.weak_value(&value).expect("NamedNodeMap is an object"),
    );
    Ok(value)
}

pub(crate) fn get_attribute_node(
    ctx: &mut Ctx,
    element: &DomNode,
    qualified_name: &str,
) -> OpResult<Value> {
    let name = normalize_element_attribute_name(element, qualified_name);
    let attribute = element
        .realm
        .session
        .borrow_mut()
        .document_mut()
        .attribute_node_by_name(element.id, &name)
        .map_err(dom_error)?;
    Ok(attribute.map_or(Value::Null, |attribute| element.realm.wrap(ctx, attribute)))
}

pub(crate) fn get_attribute_node_ns(
    ctx: &mut Ctx,
    element: &DomNode,
    namespace_uri: Option<&str>,
    local_name: &str,
) -> OpResult<Value> {
    let attribute = element
        .realm
        .session
        .borrow_mut()
        .document_mut()
        .attribute_node_by_ns(element.id, namespace_uri, local_name)
        .map_err(dom_error)?;
    Ok(attribute.map_or(Value::Null, |attribute| element.realm.wrap(ctx, attribute)))
}

pub(crate) fn set_attribute_node(
    ctx: &mut Ctx,
    element: &DomNode,
    attribute: &DomAttr,
    namespace_aware: bool,
) -> OpResult<Value> {
    let map = DomNamedNodeMap {
        realm: element.realm.clone(),
        element: element.id,
        _owner: Value::Null,
    };
    map.set_attribute_node(ctx, attribute, namespace_aware)
}

pub(crate) fn remove_attribute_node(
    ctx: &mut Ctx,
    element: &DomNode,
    attribute: &DomAttr,
) -> OpResult<Value> {
    let map = DomNamedNodeMap {
        realm: element.realm.clone(),
        element: element.id,
        _owner: Value::Null,
    };
    map.remove_attribute_node(ctx, attribute.base.id)
}

pub(crate) fn create_attribute(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    qualified_name: &str,
) -> OpResult<Value> {
    if !lumen_html::xml::is_valid_attribute_local_name(qualified_name) {
        return Err(error_reporting::dom_exception(
            ctx,
            "InvalidCharacterError",
            "attribute name is not a valid attribute local name",
        ));
    }
    let name = if realm.is_html_document {
        qualified_name.to_ascii_lowercase()
    } else {
        qualified_name.to_owned()
    };
    let id = realm
        .session
        .borrow_mut()
        .document_mut()
        .create_attribute(None, &name, "")
        .map_err(dom_error)?;
    Ok(realm.wrap(ctx, id))
}

pub(crate) fn create_attribute_ns(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    namespace_uri: Option<&str>,
    qualified_name: &str,
) -> OpResult<Value> {
    // Attribute and element local names have different DOM validation rules.
    let _ = super::namespace_for_qname(ctx, namespace_uri, qualified_name, lumen_html::xml::DomNameContext::Attribute)?;
    let namespace_uri = namespace_uri.filter(|namespace| !namespace.is_empty());
    let id = realm
        .session
        .borrow_mut()
        .document_mut()
        .create_attribute(namespace_uri, qualified_name, "")
        .map_err(dom_error)?;
    Ok(realm.wrap(ctx, id))
}

fn normalize_element_attribute_name(element: &DomNode, name: &str) -> String {
    if element.realm.is_html_document
        && matches!(
            element.realm.session.borrow().document().kind(element.id),
            Ok(NodeKind::Element {
                namespace: Namespace::Html,
                ..
            })
        )
    {
        name.to_ascii_lowercase()
    } else {
        name.to_owned()
    }
}

fn set_attr_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    attribute: NodeId,
    value: &str,
) -> OpResult<()> {
    let (owner, namespace_uri, qualified_name) = match realm
        .session
        .borrow()
        .document()
        .kind(attribute)
        .map_err(dom_error)?
    {
        NodeKind::Attribute {
            owner_element,
            namespace_uri,
            qualified_name,
            ..
        } => (
            owner_element.as_deref().copied(),
            namespace_uri.as_deref().map(|uri| uri.as_ref().to_owned()),
            qualified_name.as_str().to_owned(),
        ),
        _ => {
            return Err(OpError::new(
                "InvalidStateError",
                "Attr node is no longer available",
            ));
        }
    };
    if namespace_uri.is_none() {
        if let Some(owner) = owner {
            forms::prepare_input_attribute_change(realm, owner, &qualified_name)?;
        }
    }
    realm
        .session
        .borrow_mut()
        .document_mut()
        .set_attribute_node_value(attribute, value)
        .map_err(dom_error)?;
    let Some(owner) = owner else {
        return Ok(());
    };
    if namespace_uri.is_none() {
        forms::resanitize_input_after_attribute_change(realm, owner, &qualified_name)?;
    }
    if namespace_uri.is_none() && matches!(qualified_name.as_str(), "width" | "height") {
        realm.sync_canvas()?;
    }
    realm.sync_image_bitmaps()?;
    event_content_handlers::attribute_changed(
        ctx,
        realm,
        owner,
        namespace_uri.as_deref(),
        &qualified_name,
        Some(value),
    )?;
    realm.flush_script_activations(ctx)
}

fn attribute_error(ctx: &mut Ctx, error: Error) -> OpError {
    match error {
        Error::InUseAttribute => in_use_attribute(ctx),
        Error::NotFound => error_reporting::dom_exception(
            ctx,
            "NotFoundError",
            "the requested attribute is not present on this element",
        ),
        error => dom_error(error),
    }
}

fn in_use_attribute(ctx: &mut Ctx) -> OpError {
    error_reporting::dom_exception(
        ctx,
        "InUseAttributeError",
        "the Attr is already associated with another Element",
    )
}

fn missing_attribute(ctx: &mut Ctx, name: &str) -> OpError {
    error_reporting::dom_exception(
        ctx,
        "NotFoundError",
        &format!("no attribute named {name:?} is present"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Completion;
    use lumen_runtime::Runtime;

    fn eval_value_or_panic(engine: &mut lumen::Engine, source: &str, contract: &str) -> Value {
        match engine.eval_value(source) {
            Ok(Ok(value)) => value,
            Ok(Err(thrown)) => match engine.describe_throw(thrown) {
                Completion::Throw { name, message } => {
                    panic!("{contract} threw {name}: {message}")
                }
                Completion::Value(message) => panic!("{contract} threw: {message}"),
            },
            Err(error) => panic!("{contract} did not parse: {error:?}"),
        }
    }

    #[test]
    fn named_node_map_properties_are_live_unenumerable_and_branded() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        super::super::install(engine.ctx(), "", 128).unwrap();
        let result = eval_value_or_panic(
            engine,
            r#"(() => {
                    const element = document.createElement('div');
                    element.setAttribute('first', 'one');
                    element.setAttribute('item', 'attribute');
                    element.setAttribute('length', 'attribute');
                    element.setAttributeNS(null, 'toString', 'attribute');
                    element.setAttribute('tostring', 'visible');
                    const map = element.attributes;
                    Object.defineProperty(map, 'ownshadow', {
                        value: 'own', configurable: true, enumerable: true
                    });
                    element.setAttribute('ownshadow', 'attribute');
                    const first = map[0];
                    const firstDescriptor = Object.getOwnPropertyDescriptor(map, 'first');
                    const ownNames = Object.getOwnPropertyNames(map);
                    element.setAttribute('second', 'two');
                    let brandRejected = false;
                    try {
                        NamedNodeMap.prototype.item.call(new Proxy(map, {}), 0);
                    } catch (error) {
                        brandRejected = error instanceof TypeError;
                    }
                    if (!(map.first === first)) return 'named getter identity';
                    if (!(map.second === map[6])) return 'live append/index identity';
                    if (!(map.ownshadow === 'own')) return 'expando shadows supported name';
                    if (!(map.tostring === map[4] && map.tostring.value === 'visible')) {
                        return 'lowercase named property visibility';
                    }
                    if (!(firstDescriptor.value === first && firstDescriptor.writable === false &&
                        firstDescriptor.enumerable === false && firstDescriptor.configurable === true)) {
                        return 'named property descriptor';
                    }
                    if (!(JSON.stringify(ownNames) ===
                        JSON.stringify(['0', '1', '2', '3', '4', '5', 'first', 'tostring', 'ownshadow']))) {
                        return 'own property names';
                    }
                    if (!(JSON.stringify(Object.keys(map)) ===
                        JSON.stringify(['0', '1', '2', '3', '4', '5', '6', 'ownshadow']))) {
                        return 'enumerable own keys';
                    }
                    if (!(map.item === NamedNodeMap.prototype.item && typeof map.item === 'function')) {
                        return 'prototype method collision';
                    }
                    if (!(map.length === 7 && typeof map.toString === 'function')) {
                        return 'length and stringifier collision';
                    }
                    if ('named_supported' in NamedNodeMap.prototype) return 'hidden native hook';
                    if (!(Reflect.has(map, 'first') && !Reflect.has(map, 'missing'))) return 'Reflect.has';
                    if (!(Reflect.set(map, 'first', 1) === false)) return 'Reflect.set named property';
                    if (!(Reflect.defineProperty(map, 'first', {value: 1}) === false)) {
                        return 'Reflect.defineProperty named property';
                    }
                    if (!(Reflect.defineProperty(map, 'item', {value: 1}) === false)) {
                        return 'Reflect.defineProperty prototype collision';
                    }
                    if (!(Reflect.deleteProperty(map, 'first') === false)) return 'Reflect.deleteProperty';
                    if (!(Reflect.preventExtensions(map) === false && Object.isExtensible(map))) {
                        return 'extensibility';
                    }
                    if (!brandRejected) return 'author Proxy brand rejection';
                    return true;
                })()"#,
            "NamedNodeMap adapter contract",
        );
        match result {
            Value::Bool(true) => {}
            Value::Str(label) => panic!("NamedNodeMap check failed: {label}"),
            _ => panic!("NamedNodeMap contract returned an unexpected value"),
        }
    }

    #[test]
    fn large_named_node_map_keys_keep_order_deduplicate_and_stay_lazy() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm =
            super::super::install(engine.ctx(), "<body><div id=large-map></div></body>", 2048)
                .unwrap();
        let result = eval_value_or_panic(
            engine,
            r#"(() => {
                    const element = document.getElementById('large-map');
                    for (let i = 0; i < 512; i++) {
                        const name = 'data-' + ('0000' + i).slice(-4);
                        element.setAttributeNS('urn:first', name, 'first');
                        element.setAttributeNS('urn:second', name, 'second');
                    }
                    const map = element.attributes;
                    const keys = Reflect.ownKeys(map);
                    if (keys.length !== 1538 || map.length !== 1025) return false;
                    for (let i = 0; i < 1025; i++) {
                        if (keys[i] !== String(i)) return false;
                    }
                    if (keys[1025] !== 'id') return false;
                    for (let i = 0; i < 512; i++) {
                        const expected = 'data-' + ('0000' + i).slice(-4);
                        if (keys[1026 + i] !== expected) return false;
                    }
                    return true;
                })()"#,
            "large NamedNodeMap ownKeys contract",
        );
        assert!(matches!(result, Value::Bool(true)));

        let session = realm.session_handle();
        let session = session.borrow();
        let document = session.document();
        let element = lumen_html::selector::query_selector(document, document.root(), "#large-map")
            .unwrap()
            .expect("fixture element remains in the document");
        assert!(
            document.materialized_attribute_nodes(element).is_none(),
            "enumerating NamedNodeMap keys must not materialize Attr nodes"
        );
    }
}
