//! Live `HTMLElement.dataset` / `DOMStringMap` facades.
//!
//! The Web IDL named-property behavior is represented by a JavaScript Proxy, while every read and
//! mutation is performed by the typed native backend against the element's live attribute list.
use crate::{DomNode, DomRealm};
use lumen::embed::{Ctx, OpError, OpResult, Value, WeakValue};
use lumen_html::{NodeId, NodeKind};
use std::rc::Rc;

#[derive(Clone)]
pub(crate) struct ProxyIntrinsics {
    constructor: WeakValue,
    prototype: WeakValue,
    reflect_get: WeakValue,
    reflect_set: WeakValue,
    reflect_delete: WeakValue,
    reflect_has: WeakValue,
    reflect_own_keys: WeakValue,
    reflect_get_own_property_descriptor: WeakValue,
}

pub(crate) fn install(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> Result<(), Value> {
    let global = ctx.global_this();
    let proxy_constructor = ctx.member_get(&global, "Proxy")?;
    let reflect = ctx.member_get(&global, "Reflect")?;
    let reflect_get = ctx.member_get(&reflect, "get")?;
    let reflect_set = ctx.member_get(&reflect, "set")?;
    let reflect_delete = ctx.member_get(&reflect, "deleteProperty")?;
    let reflect_has = ctx.member_get(&reflect, "has")?;
    let reflect_own_keys = ctx.member_get(&reflect, "ownKeys")?;
    let reflect_get_own_property_descriptor =
        ctx.member_get(&reflect, "getOwnPropertyDescriptor")?;
    let constructor = ctx.class_constructor::<crate::DomDomStringMap>();
    let prototype = ctx.member_get(&constructor, "prototype")?;
    *realm.dataset_intrinsics.borrow_mut() = Some(ProxyIntrinsics {
        constructor: crate::realm_services::capture_realm_value(ctx,proxy_constructor).map_err(|error|error.to_value(ctx))?,
        prototype: crate::realm_services::capture_realm_value(ctx,prototype).map_err(|error|error.to_value(ctx))?,
        reflect_get: crate::realm_services::capture_realm_value(ctx,reflect_get).map_err(|error|error.to_value(ctx))?,
        reflect_set: crate::realm_services::capture_realm_value(ctx,reflect_set).map_err(|error|error.to_value(ctx))?,
        reflect_delete: crate::realm_services::capture_realm_value(ctx,reflect_delete).map_err(|error|error.to_value(ctx))?,
        reflect_has: crate::realm_services::capture_realm_value(ctx,reflect_has).map_err(|error|error.to_value(ctx))?,
        reflect_own_keys: crate::realm_services::capture_realm_value(ctx,reflect_own_keys).map_err(|error|error.to_value(ctx))?,
        reflect_get_own_property_descriptor: crate::realm_services::capture_realm_value(ctx,reflect_get_own_property_descriptor).map_err(|error|error.to_value(ctx))?,
    });
    Ok(())
}

pub(crate) fn for_element(ctx: &mut Ctx, node: &DomNode) -> OpResult<Value> {
    if let Some(value) = node
        .collections
        .borrow()
        .get("dataset")
        .and_then(WeakValue::upgrade)
    {
        return Ok(value);
    }
    let intrinsics = node
        .realm
        .dataset_intrinsics
        .borrow()
        .clone()
        .ok_or_else(|| OpError::new("InvalidStateError", "DOMStringMap is not initialized"))?;
    // Keep the canonical element facade alive while DOMStringMap is held. Its backend stores a
    // NodeId, but a detached-node sweep is allowed to reclaim that node when no facade survives.
    // The cached wrapper is canonical (so adoption and expando identity remain shared) and does
    // not create a cycle: the element caches only a weak reference to this proxy.
    let element_wrapper = node.realm.wrap(ctx, node.id);
    let backend = ctx.new_instance(DomStringMapBackend {
        realm: node.realm.clone(),
        element: node.id,
        _element_wrapper: element_wrapper,
    });
    ctx.set_native_identity_owner::<DomStringMapBackend>(&backend)?;
    let handler = DomStringMapProxyHandler {
        backend: backend.clone(),
        has_named: method(ctx, &backend, "hasNamedProperty").map_err(OpError::thrown)?,
        get_named: method(ctx, &backend, "getNamedProperty").map_err(OpError::thrown)?,
        set_named: method(ctx, &backend, "setNamedProperty").map_err(OpError::thrown)?,
        delete_named: method(ctx, &backend, "deleteNamedProperty").map_err(OpError::thrown)?,
        supported_names: method(ctx, &backend, "supportedPropertyNames")
            .map_err(OpError::thrown)?,
        reflect_get: intrinsics.reflect_get.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
        reflect_set: intrinsics.reflect_set.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
        reflect_delete: intrinsics.reflect_delete.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
        reflect_has: intrinsics.reflect_has.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
        reflect_own_keys: intrinsics.reflect_own_keys.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
        reflect_get_own_property_descriptor: intrinsics.reflect_get_own_property_descriptor.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?,
    };
    let handler = ctx.new_instance(handler);
    ctx.set_native_identity_owner::<DomStringMapProxyHandler>(&handler)?;
    let target = ctx.new_object_with_proto(&intrinsics.prototype.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset prototype realm is unavailable"))?);
    let proxy = ctx
        .construct_value(intrinsics.constructor.upgrade().ok_or_else(||OpError::new("InvalidStateError","dataset intrinsic realm is unavailable"))?, &[target, handler])
        .map_err(OpError::thrown)?;
    node.collections.borrow_mut().insert(
        "dataset".into(),
        ctx.weak_value(&proxy).expect("dataset proxy"),
    );
    Ok(proxy)
}

fn method(ctx: &mut Ctx, backend: &Value, name: &str) -> Result<Value, Value> {
    ctx.member_get(backend, name)
}

fn call(ctx: &mut Ctx, function: &Value, receiver: Value, args: &[Value]) -> OpResult<Value> {
    ctx.invoke(function.clone(), receiver, args)
        .map_err(OpError::thrown)
}

fn backend_has(ctx: &mut Ctx, backend: &Value, has: &Value, key: &Value) -> OpResult<bool> {
    if !matches!(key, Value::Str(_)) {
        return Ok(false);
    }
    Ok(matches!(
        call(ctx, has, backend.clone(), std::slice::from_ref(key))?,
        Value::Bool(true)
    ))
}

fn array_values(ctx: &mut Ctx, array: &Value) -> OpResult<Vec<Value>> {
    let length = ctx.member_get(array, "length")?;
    let Value::Num(length) = length else {
        return Err(OpError::new(
            "TypeError",
            "proxy trap produced a non-array key list",
        ));
    };
    if !length.is_finite() || length < 0.0 || length > 65_536.0 || length.fract() != 0.0 {
        return Err(OpError::new(
            "RangeError",
            "proxy trap key list is too large",
        ));
    }
    let mut values = Vec::with_capacity(length as usize);
    for index in 0..length as usize {
        values.push(ctx.member_get(array, &index.to_string())?);
    }
    Ok(values)
}

fn same_property_key(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Str(left), Value::Str(right)) => left.as_str() == right.as_str(),
        (Value::Sym(left), Value::Sym(right)) => left.id == right.id,
        _ => false,
    }
}

#[lumen_bind::class(name = "DOMStringMapProxyHandler")]
struct DomStringMapProxyHandler {
    backend: Value,
    has_named: Value,
    get_named: Value,
    set_named: Value,
    delete_named: Value,
    supported_names: Value,
    reflect_get: Value,
    reflect_set: Value,
    reflect_delete: Value,
    reflect_has: Value,
    reflect_own_keys: Value,
    reflect_get_own_property_descriptor: Value,
}

impl lumen::embed::NativeIdentityOwner for DomStringMapProxyHandler {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit:&mut dyn FnMut(&Value)) {
        for value in [&self.backend,&self.has_named,&self.get_named,&self.set_named,&self.delete_named,
            &self.supported_names,&self.reflect_get,&self.reflect_set,&self.reflect_delete,&self.reflect_has,
            &self.reflect_own_keys,&self.reflect_get_own_property_descriptor] {visit(value);}
    }
}

#[lumen_bind::methods]
impl DomStringMapProxyHandler {
    #[method(name = "get")]
    fn get_property(
        &self,
        ctx: &mut Ctx,
        target: Value,
        key: Value,
        receiver: Value,
    ) -> OpResult<Value> {
        if backend_has(ctx, &self.backend, &self.has_named, &key)? {
            return call(ctx, &self.get_named, self.backend.clone(), &[key]);
        }
        call(
            ctx,
            &self.reflect_get,
            Value::Undefined,
            &[target, key, receiver],
        )
    }

    #[method(name = "set")]
    fn set_property(
        &self,
        ctx: &mut Ctx,
        target: Value,
        key: Value,
        value: Value,
        receiver: Value,
    ) -> OpResult<Value> {
        if matches!(&key, Value::Str(_)) {
            call(ctx, &self.set_named, self.backend.clone(), &[key, value])?;
            return Ok(Value::Bool(true));
        }
        call(
            ctx,
            &self.reflect_set,
            Value::Undefined,
            &[target, key, value, receiver],
        )
    }

    #[method(name = "deleteProperty")]
    fn delete_property(&self, ctx: &mut Ctx, target: Value, key: Value) -> OpResult<Value> {
        if backend_has(ctx, &self.backend, &self.has_named, &key)? {
            call(ctx, &self.delete_named, self.backend.clone(), &[key])?;
            return Ok(Value::Bool(true));
        }
        call(ctx, &self.reflect_delete, Value::Undefined, &[target, key])
    }

    #[method(name = "has")]
    fn has_property(&self, ctx: &mut Ctx, target: Value, key: Value) -> OpResult<Value> {
        if backend_has(ctx, &self.backend, &self.has_named, &key)? {
            return Ok(Value::Bool(true));
        }
        call(ctx, &self.reflect_has, Value::Undefined, &[target, key])
    }

    #[method(name = "ownKeys")]
    fn own_keys(&self, ctx: &mut Ctx, target: Value) -> OpResult<Value> {
        let names = call(ctx, &self.supported_names, self.backend.clone(), &[])?;
        let target_keys = call(ctx, &self.reflect_own_keys, Value::Undefined, &[target])?;
        let mut keys = array_values(ctx, &names)?;
        for key in array_values(ctx, &target_keys)? {
            if !keys
                .iter()
                .any(|existing| same_property_key(existing, &key))
            {
                keys.push(key);
            }
        }
        Ok(ctx.make_array(keys))
    }

    #[method(name = "getOwnPropertyDescriptor")]
    fn get_own_property_descriptor(
        &self,
        ctx: &mut Ctx,
        target: Value,
        key: Value,
    ) -> OpResult<Value> {
        if backend_has(ctx, &self.backend, &self.has_named, &key)? {
            let value = call(ctx, &self.get_named, self.backend.clone(), &[key])?;
            let descriptor = Value::Obj(ctx.new_object());
            for (name, value) in [
                ("value", value),
                ("writable", Value::Bool(true)),
                ("enumerable", Value::Bool(true)),
                ("configurable", Value::Bool(true)),
            ] {
                ctx.member_set(&descriptor, name, value)?;
            }
            return Ok(descriptor);
        }
        call(
            ctx,
            &self.reflect_get_own_property_descriptor,
            Value::Undefined,
            &[target, key],
        )
    }
}

#[lumen_bind::class(name = "DOMStringMapBackend")]
struct DomStringMapBackend {
    realm: Rc<DomRealm>,
    element: NodeId,
    _element_wrapper: Value,
}

impl lumen::embed::NativeIdentityOwner for DomStringMapBackend {
    const TRACES_NATIVE_VALUES: bool = true;
    fn trace_native_identities(&self, _:u64, _: &mut dyn FnMut(&Value)) {}
    fn trace_native_values(&self, visit:&mut dyn FnMut(&Value)) {visit(&self._element_wrapper);}
}

impl DomStringMapBackend {
    fn element(&self) -> (Rc<DomRealm>, NodeId) {
        self.realm.resolve_adopted_node(self.element)
    }
}

#[lumen_bind::methods]
impl DomStringMapBackend {
    #[method(name = "hasNamedProperty", coerce)]
    fn has_named_property(&self, name: &str) -> OpResult<bool> {
        let Some(attribute) = attribute_for_property_name(name) else {
            return Ok(false);
        };
        let (realm, element) = self.element();
        let session = realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(element).map_err(crate::dom_error)?
        else {
            return Ok(false);
        };
        Ok(attributes
            .iter()
            .any(|(name, _)| name.as_str() == attribute))
    }

    #[method(name = "getNamedProperty", coerce)]
    fn get_named_property(&self, name: &str) -> OpResult<Value> {
        let Some(attribute) = attribute_for_property_name(name) else {
            return Ok(Value::Undefined);
        };
        let (realm, element) = self.element();
        let session = realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(element).map_err(crate::dom_error)?
        else {
            return Ok(Value::Undefined);
        };
        Ok(attributes
            .iter()
            .find(|(name, _)| name.as_str() == attribute)
            .map_or(Value::Undefined, |(_, value)| {
                Value::from_string(value.clone())
            }))
    }

    #[method(name = "setNamedProperty", coerce, hint(js(ce_reactions)))]
    fn set_named_property(&self, ctx: &mut Ctx, name: &str, value: &str) -> OpResult<()> {
        let attribute = attribute_for_set(ctx, name)?;
        let (realm, element) = self.element();
        let result = realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(element, None, &attribute, value);
        result.map_err(crate::dom_error)
    }

    #[method(name = "deleteNamedProperty", coerce, hint(js(ce_reactions)))]
    fn delete_named_property(&self, name: &str) -> OpResult<()> {
        let Some(attribute) = attribute_for_property_name(name) else {
            return Ok(());
        };
        let (realm, element) = self.element();
        let result = realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(element, &attribute);
        result.map_err(crate::dom_error)
    }

    #[method(name = "supportedPropertyNames")]
    fn supported_property_names(&self) -> OpResult<Vec<String>> {
        let (realm, element) = self.element();
        let session = realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(element).map_err(crate::dom_error)?
        else {
            return Ok(Vec::new());
        };
        Ok(attributes
            .iter()
            .filter_map(|(name, _)| property_name_for_attribute(name.as_str()))
            .collect())
    }
}

fn property_name_for_attribute(attribute: &str) -> Option<String> {
    let suffix = attribute.strip_prefix("data-")?;
    if suffix.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return None;
    }
    let mut result = String::with_capacity(suffix.len());
    let mut characters = suffix.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '-'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            result.push(characters.next()?.to_ascii_uppercase());
        } else {
            result.push(character);
        }
    }
    Some(result)
}

fn attribute_for_property_name(name: &str) -> Option<String> {
    let mut attribute = String::with_capacity(5 + name.len());
    attribute.push_str("data-");
    let mut characters = name.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '-'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            return None;
        }
        if character.is_ascii_uppercase() {
            attribute.push('-');
            attribute.push(character.to_ascii_lowercase());
        } else {
            attribute.push(character);
        }
    }
    Some(attribute)
}

fn attribute_for_set(ctx: &mut Ctx, name: &str) -> OpResult<String> {
    let mut attribute = String::with_capacity(5 + name.len());
    attribute.push_str("data-");
    let mut characters = name.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '-'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            return Err(crate::error_reporting::dom_exception(
                ctx,
                "SyntaxError",
                "dataset property name cannot contain a hyphen followed by a lowercase ASCII letter",
            ));
        }
        if character.is_ascii_uppercase() {
            attribute.push('-');
            attribute.push(character.to_ascii_lowercase());
        } else {
            attribute.push(character);
        }
    }
    if !lumen_html::xml::is_valid_attribute_local_name(&attribute) {
        return Err(crate::error_reporting::dom_exception(
            ctx,
            "InvalidCharacterError",
            "dataset property name does not produce a valid attribute name",
        ));
    }
    Ok(attribute)
}

#[cfg(test)]
mod tests {
    #[test]
    fn specification_dataset_namespace_mutation_and_foreign_same_object_adoption() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<!doctype html><body></body>", 192).unwrap();
        let result = engine.eval_value(r#"(() => {
            const check=(value,label)=>{if(!value)throw Error(label)};
            for(const namespace of ["http://www.w3.org/1999/xhtml","http://www.w3.org/2000/svg","http://www.w3.org/1998/Math/MathML"]) {
                const element=document.createElementNS(namespace,namespace.endsWith("svg")?"svg":namespace.endsWith("MathML")?"math":"div");
                const data=element.dataset;
                check(data instanceof DOMStringMap && data===element.dataset,"same native DOMStringMap");
                element.setAttributeNS("urn:first","data-value","first");
                element.setAttributeNS("urn:second","data-value","second");
                data.value="third";
                check(element.attributes.length===3 && element.getAttributeNS("urn:first","data-value")==="first" && element.getAttributeNS("urn:second","data-value")==="second" && element.getAttributeNS(null,"data-value")==="third","dataset setter selects the null namespace");
                element.removeAttributeNS("urn:first","data-value");element.removeAttributeNS("urn:second","data-value");
                const donor=document.implementation.createHTMLDocument("donor");donor.body.append(element);
                check(data===element.dataset && data.value==="third","held map follows actual owner adoption");
                data.liveValue="after adoption";check(element.getAttribute("data-live-value")==="after adoption","live adopted backing");
                delete data.liveValue;check(!element.hasAttribute("data-live-value"),"real attribute deletion");
                let rejected=false;try{data["bad-name"]="invalid"}catch(error){rejected=error instanceof DOMException && error.name==="SyntaxError"}check(rejected,"shared property-name conversion");
            }
            check(document.createElementNS("urn:other","other").dataset===undefined,"mixin applies only to supported namespaces");
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("dataset guard: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,lumen::embed::Value::Bool(true)));
    }
}
