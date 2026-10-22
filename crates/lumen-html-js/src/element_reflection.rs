//! HTML reflected Element? association state, allocated only for explicit targets.
use super::*;
use lumen::embed::JsHost;
use lumen_bind::Host;

#[derive(Default)]
pub(crate) struct ReflectedElements {
    explicit: HashMap<(NodeId, &'static str), WeakValue>,
    lists: HashMap<(NodeId, &'static str), Vec<WeakValue>>,
    arrays: HashMap<(NodeId, &'static str), (WeakValue, Vec<NodeId>)>,
}

impl ReflectedElements {
    pub(crate) fn attribute_changed(&mut self, node: NodeId, namespace: Option<&str>, local: &str) {
        if namespace.is_none() {
            self.explicit.retain(|(owner, attribute), _| *owner != node || *attribute != local);
            self.lists.retain(|(owner, attribute), _| *owner != node || *attribute != local);
            self.arrays.retain(|(owner, attribute), _| *owner != node || *attribute != local);
        }
    }

    pub(crate) fn reap(&mut self, document: &lumen_html::Document) {
        self.explicit.retain(|(owner, _), value| document.kind(*owner).is_ok() && value.upgrade().is_some());
        self.lists.retain(|(owner, _), _| document.kind(*owner).is_ok());
        self.arrays.retain(|(owner, _), (array, _)| document.kind(*owner).is_ok() && array.upgrade().is_some());
    }

    pub(crate) fn move_nodes(&mut self, destination: &mut Self, mapping: &[(NodeId, NodeId)]) -> OpResult<()> {
        let count = self.explicit.keys().filter(|(node, _)| mapping.iter().any(|(old, _)| old == node)).count();
        destination.explicit.try_reserve(count).map_err(|_| OpError::new("QuotaExceededError", "reflected target adoption allocation"))?;
        let mut moved = Vec::new();
        moved.try_reserve(count).map_err(|_| OpError::new("QuotaExceededError", "reflected target adoption allocation"))?;
        self.explicit.retain(|(node, attribute), value| {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| old == node) { moved.push(((*new, *attribute), value.clone())); false } else { true }
        });
        destination.explicit.extend(moved);
        let mut moved_lists = Vec::new();
        self.lists.retain(|(node, attribute), value| {
            if let Some((_, new)) = mapping.iter().find(|(old, _)| old == node) { moved_lists.push(((*new, *attribute), value.clone())); false } else { true }
        });
        destination.lists.extend(moved_lists);
        Ok(())
    }

    pub(crate) fn remap_nodes(&mut self, mapping: &[(NodeId, NodeId)]) -> OpResult<()> {
        let mut moved = Self::default();
        self.move_nodes(&mut moved, mapping)?;
        self.explicit.extend(moved.explicit);
        self.lists.extend(moved.lists);
        Ok(())
    }

    pub(crate) fn set(&mut self, ctx: &mut Ctx, owner: NodeId, attribute: &'static str, value: Option<&Value>) -> OpResult<()> {
        match value {
            None => { self.explicit.remove(&(owner, attribute)); }
            Some(value) => {
                self.explicit.try_reserve(1).map_err(|_| OpError::new("QuotaExceededError", "reflected target allocation"))?;
                let weak = ctx.weak_value(value).ok_or_else(|| OpError::type_error("reflected target must be an Element"))?;
                self.explicit.insert((owner, attribute), weak);
            }
        }
        Ok(())
    }

    pub(crate) fn get(&self, ctx: &mut Ctx, realm: &Rc<DomRealm>, owner: NodeId, attribute: &'static str) -> OpResult<Value> {
        if let Some(reference) = self.explicit.get(&(owner, attribute)) {
            let Some(value) = reference.upgrade() else { return Ok(Value::Null); };
            let (target_realm, target) = ctx.with_instance::<DomElement, _>(&value,
                |element| element.base.realm.resolve_adopted_node(element.base.id))?;
            if !Rc::ptr_eq(realm, &target_realm) { return Ok(Value::Null); }
            let permitted = {
                let session = realm.session.borrow();
                explicit_target_in_scope(session.document(), owner, target).map_err(dom_error)?
            };
            return Ok(if permitted { value } else { Value::Null });
        }
        let target = {
            let session = realm.session.borrow();
            let document = session.document();
            let Some(id) = document.get_attribute_ns_ref(owner, None, attribute).map_err(dom_error)? else { return Ok(Value::Null); };
            find_by_id(document, owner, id).map_err(dom_error)?
        };
        Ok(realm.wrap_option(ctx, target))
    }
}

fn find_by_id(document: &lumen_html::Document, owner: NodeId, id: &str) -> Result<Option<NodeId>, Error> {
    let root = document.root_node(owner, false)?;
    if id.is_empty() { return Ok(None); }
    if matches!(document.kind(root)?, NodeKind::Element { .. }) && document.get_attribute_ns_ref(root, None, "id")? == Some(id) { return Ok(Some(root)); }
    lumen_html::selector::get_element_by_id(document, root, id)
}

pub(crate) fn install(realm: &Rc<DomRealm>) {
    let weak = Rc::downgrade(realm);
    realm.add_mutation_sink(Rc::new(move |document, mutation| {
        let Some(realm) = weak.upgrade() else { return; };
        if let lumen_html::observe::ObservedKind::Attribute { name, namespace_uri, old_value } = &mutation.kind {
            realm.reflected_elements.borrow_mut().attribute_changed(mutation.target, namespace_uri.as_deref(), name);
            if namespace_uri.is_none() && name == "open" {
                super::dialog_popover::dialog_attribute_changed(document, &realm, mutation.target, old_value.as_deref());
            }
        }
    }));
}

pub(crate) fn get(ctx: &mut Ctx, owner: Value, attribute: &'static str) -> OpResult<Value> {
    let (realm, node) = ctx.with_instance::<DomElement, _>(&owner, |element| element.base.realm.resolve_adopted_node(element.base.id))?;
    let result = realm.reflected_elements.borrow().get(ctx, &realm, node, attribute);
    result
}

pub(crate) fn set(ctx: &mut Ctx, owner: Value, attribute: &'static str, value: Value) -> OpResult<()> {
    let value = nullable_element(ctx, value)?;
    let (realm, node) = ctx.with_instance::<DomElement, _>(&owner, |element| element.base.realm.resolve_adopted_node(element.base.id))?;
    // Reserve the sparse slot before changing content. The mutation observer
    // clears an older explicit reference; publish the new one afterwards.
    if value.is_some() { realm.reflected_elements.borrow_mut().explicit.try_reserve(1)
        .map_err(|_| OpError::new("QuotaExceededError", "reflected target allocation"))?; }
    let native = super::tables::node_at(&realm, node);
    if value.is_some() { native.set_attribute_core(attribute, "")?; }
    else { native.remove_attribute_core(attribute)?; }
    let result = realm.reflected_elements.borrow_mut().set(ctx, node, attribute, value.as_ref());
    result
}

fn explicit_target_in_scope(document: &lumen_html::Document, owner: NodeId, target: NodeId) -> Result<bool, Error> {
    let mut ancestor = document.shadow_including_parent(owner)?;
    while let Some(ancestor_node) = ancestor {
        let mut parent = document.parent(target)?;
        while let Some(candidate) = parent {
            if candidate == ancestor_node { return Ok(true); }
            parent = document.parent(candidate)?;
        }
        ancestor = document.shadow_including_parent(ancestor_node)?;
    }
    Ok(false)
}

pub(crate) fn nullable_element(ctx: &mut Ctx, value: Value) -> OpResult<Option<Value>> {
    if matches!(value, Value::Null | Value::Undefined) { return Ok(None); }
    ctx.with_instance::<DomElement, _>(&value, |_| ())
        .map_err(|_| OpError::type_error("value must be an Element or null"))?;
    Ok(Some(value))
}

/// Event source retargeting uses the same core DOM algorithm as dispatch.
pub(crate) fn retarget_event_source(ctx: &mut Ctx, source: Value, current_target: Option<Value>) -> Value {
    let Some((realm, source_node)) = ctx.with_instance::<DomElement, _>(&source,
        |element| element.base.realm.resolve_adopted_node(element.base.id)).ok() else { return source; };
    let context = current_target.as_ref().and_then(|target|
        ctx.with_instance::<DomElement, _>(target, |element| element.base.realm.resolve_adopted_node(element.base.id)).ok())
        .and_then(|(target_realm, target)| Rc::ptr_eq(&realm, &target_realm).then_some(target));
    let retargeted = realm.session.borrow().document().retarget(source_node, context).ok();
    retargeted.map_or(source, |node| realm.wrap(ctx, node))
}

pub(crate) fn get_list(ctx: &mut Ctx, owner: Value, attribute: &'static str) -> OpResult<Value> {
    let (realm, node) = ctx.with_instance::<DomElement, _>(&owner, |element| element.base.realm.resolve_adopted_node(element.base.id))?;
    let explicit = realm.reflected_elements.borrow().lists.get(&(node, attribute)).cloned();
    let mut values = Vec::new();
    let mut nodes = Vec::new();
    if let Some(explicit) = explicit {
        for value in explicit.iter().filter_map(WeakValue::upgrade) {
            let (target_realm, target) = ctx.with_instance::<DomElement, _>(&value,
                |element| element.base.realm.resolve_adopted_node(element.base.id))?;
            if !Rc::ptr_eq(&realm, &target_realm) { continue; }
            let permitted = {
                let session = realm.session.borrow();
                explicit_target_in_scope(session.document(), node, target).map_err(dom_error)?
            };
            if permitted { values.push(value); nodes.push(target); }
        }
    } else {
        let targets = {
            let session = realm.session.borrow();
            let document = session.document();
            let Some(ids) = document.get_attribute_ns_ref(node, None, attribute).map_err(dom_error)? else { return Ok(Value::Null); };
            let mut targets = Vec::new();
            for id in ids.split(|character: char| matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' ')).filter(|id| !id.is_empty()) {
                if let Some(target) = find_by_id(document, node, id).map_err(dom_error)? { targets.push(target); }
            }
            targets
        };
        for target in targets { values.push(realm.wrap(ctx, target)); nodes.push(target); }
    }
    let cached = realm.reflected_elements.borrow().arrays.get(&(node, attribute))
        .filter(|(_, previous)| *previous == nodes)
        .and_then(|(array, _)| array.upgrade());
    if let Some(array) = cached { return Ok(array); }
    let array = JsHost::from_list(ctx, values);
    ctx.freeze_native_object(&array);
    if let Some(weak) = ctx.weak_value(&array) {
        realm.reflected_elements.borrow_mut().arrays.insert((node, attribute), (weak, nodes));
    }
    Ok(array)
}

pub(crate) fn set_list(ctx: &mut Ctx, owner: Value, attribute: &'static str, values: Option<Vec<Value>>) -> OpResult<()> {
    let (realm, node) = ctx.with_instance::<DomElement, _>(&owner, |element| element.base.realm.resolve_adopted_node(element.base.id))?;
    let native = super::tables::node_at(&realm, node);
    let Some(values) = values else {
        native.remove_attribute_core(attribute)?;
        return Ok(());
    };
    realm.reflected_elements.borrow_mut().lists.try_reserve(1)
        .map_err(|_| OpError::new("QuotaExceededError", "reflected target allocation"))?;
    native.set_attribute_core(attribute, "")?;
    let weak: Vec<WeakValue> = values.iter().filter_map(|value| ctx.weak_value(value)).collect();
    realm.reflected_elements.borrow_mut().lists.insert((node, attribute), weak);
    Ok(())
}
