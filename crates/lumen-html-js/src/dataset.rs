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
    constructor: Value,
    prototype: Value,
    reflect_get: Value,
    reflect_set: Value,
    reflect_delete: Value,
    reflect_has: Value,
    reflect_own_keys: Value,
    reflect_get_own_property_descriptor: Value,
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
        constructor: proxy_constructor,
        prototype,
        reflect_get,
        reflect_set,
        reflect_delete,
        reflect_has,
        reflect_own_keys,
        reflect_get_own_property_descriptor,
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
    let handler = DomStringMapProxyHandler {
        backend: backend.clone(),
        has_named: method(ctx, &backend, "hasNamedProperty").map_err(OpError::thrown)?,
        get_named: method(ctx, &backend, "getNamedProperty").map_err(OpError::thrown)?,
        set_named: method(ctx, &backend, "setNamedProperty").map_err(OpError::thrown)?,
        delete_named: method(ctx, &backend, "deleteNamedProperty").map_err(OpError::thrown)?,
        supported_names: method(ctx, &backend, "supportedPropertyNames")
            .map_err(OpError::thrown)?,
        reflect_get: intrinsics.reflect_get,
        reflect_set: intrinsics.reflect_set,
        reflect_delete: intrinsics.reflect_delete,
        reflect_has: intrinsics.reflect_has,
        reflect_own_keys: intrinsics.reflect_own_keys,
        reflect_get_own_property_descriptor: intrinsics.reflect_get_own_property_descriptor,
    };
    let handler = ctx.new_instance(handler);
    let target = ctx.new_object_with_proto(&intrinsics.prototype);
    let proxy = ctx
        .construct_value(intrinsics.constructor, &[target, handler])
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

    #[method(name = "setNamedProperty", coerce)]
    fn set_named_property(&self, name: &str, value: &str) -> OpResult<()> {
        let attribute = attribute_for_set(name)?;
        let (realm, element) = self.element();
        let result = realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(element, &attribute, value);
        result.map_err(crate::dom_error)
    }

    #[method(name = "deleteNamedProperty", coerce)]
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

fn attribute_for_set(name: &str) -> OpResult<String> {
    let mut attribute = String::with_capacity(5 + name.len());
    attribute.push_str("data-");
    let mut characters = name.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '-'
            && characters
                .peek()
                .is_some_and(|next| next.is_ascii_lowercase())
        {
            return Err(OpError::new(
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
    if !lumen_html::xml::is_xml_name(&attribute) {
        return Err(OpError::new(
            "InvalidCharacterError",
            "dataset property name does not produce a valid attribute name",
        ));
    }
    Ok(attribute)
}
