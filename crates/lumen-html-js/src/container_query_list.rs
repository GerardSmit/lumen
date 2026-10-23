//! `Element.matchContainer()`: a live `ContainerQueryList` whose `change` event
//! fires from the rendering opportunity when the evaluated result flips.
use super::*;
use crate::events::{DomEvent, HtmlTargetExt};
use crate::realm_services::RealmServices;
use lumen_bind::This;
use lumen_html::css;
use lumen_host::events::EventInit;
use std::{cell::Cell, rc::Weak};

struct ContainerListData {
    realm: Weak<DomRealm>,
    node: NodeId,
    query: String,
    was_matching: Cell<bool>,
    target: DomEventTarget,
    wrapper: RefCell<Option<WeakValue>>,
}

struct ContainerListRegistry {
    lists: RefCell<Vec<Weak<ContainerListData>>>,
}

fn evaluate(realm: &Rc<DomRealm>, node: NodeId, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let _ = realm.flush_layout();
    evaluate_flushed(realm, node, query)
}

fn evaluate_flushed(realm: &Rc<DomRealm>, node: NodeId, query: &str) -> bool {
    if query.is_empty() {
        return false;
    }
    let fonts = crate::canvas::initialized_realm_font_source(realm).ok().flatten();
    let text = fonts.as_ref().map(|fonts| fonts as &dyn lumen_html::paint::TextShaper);
    realm.session.borrow_mut().container_query_matches(node, query, text).unwrap_or(false)
}

#[lumen_bind::class(name = "ContainerQueryList", extends = DomEventTarget, hint(js(webidl)))]
pub(crate) struct DomContainerQueryList {
    base: DomEventTarget,
    data: Rc<ContainerListData>,
}

#[lumen_bind::methods]
impl DomContainerQueryList {
    #[getter]
    fn query(&self) -> String {
        self.data.query.clone()
    }

    #[getter]
    fn matches(&self) -> bool {
        self.data.realm.upgrade().is_some_and(|realm| evaluate(&realm, self.data.node, &self.data.query))
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

    #[getter]
    fn onchange(&self, ctx: &mut Ctx, this: This<Value>) -> OpResult<Value> {
        self.base.handler_value(ctx, &this.0, "change")
    }

    #[setter]
    fn set_onchange(&self, ctx: &mut Ctx, this: This<Value>, callback: crate::events::EventHandler) {
        self.base.set_event_handler(ctx, &this.0, "change", callback);
        *self.data.wrapper.borrow_mut() = ctx.weak_value(&this.0);
    }
}

pub(crate) fn create(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, input: &str) -> OpResult<Value> {
    let query = css::container_query::serialize_prelude(input).unwrap_or_default();
    realm.session.borrow_mut().require_container_oracle(true);
    let registry = match RealmServices::<ContainerListRegistry>::current(ctx) {
        Some(registry) => registry,
        None => RealmServices::replace_current(ctx, ContainerListRegistry { lists: RefCell::new(Vec::new()) }),
    };
    let data = Rc::new(ContainerListData {
        realm: Rc::downgrade(realm),
        node,
        was_matching: Cell::new(evaluate(realm, node, &query)),
        query,
        target: DomEventTarget::independent(realm),
        wrapper: RefCell::new(None),
    });
    {
        let mut lists = registry.lists.borrow_mut();
        lists.retain(|list| list.strong_count() > 0);
        lists.push(Rc::downgrade(&data));
    }
    let value = ctx.new_instance(DomContainerQueryList {
        base: DomEventTarget::from_data(data.target.data_handle()),
        data: data.clone(),
    });
    ctx.set_native_identity_owner::<DomEventTarget>(&value)?;
    *data.wrapper.borrow_mut() = ctx.weak_value(&value);
    Ok(value)
}

/// Re-evaluate this realm's lists and dispatch `change` for the ones whose
/// result differs from the last rendering opportunity.
pub(crate) fn notify(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let Some(registry) = RealmServices::<ContainerListRegistry>::current(ctx) else {
        return Ok(());
    };
    let snapshot = registry.lists.borrow().clone();
    let _ = realm.flush_layout();
    for weak in snapshot {
        let Some(data) = weak.upgrade() else {
            continue;
        };
        let Some(owner) = data.realm.upgrade() else {
            continue;
        };
        if !Rc::ptr_eq(&owner, realm) {
            continue;
        }
        let matches = evaluate_flushed(&owner, data.node, &data.query);
        if data.was_matching.replace(matches) == matches {
            continue;
        }
        let Some(wrapper) = data.wrapper.borrow().as_ref().and_then(WeakValue::upgrade) else {
            continue;
        };
        let event = ctx.new_instance(DomEvent::from_init("change", EventInit::default()));
        if let Some(event) = JsObject::from_value(event) {
            let _ = crate::events::dispatch_event(ctx, This(wrapper), event);
        }
    }
    let mut lists = registry.lists.borrow_mut();
    lists.retain(|list| list.strong_count() > 0);
    let realm_has_lists = lists.iter().filter_map(Weak::upgrade).any(|data| data.realm.upgrade().is_some_and(|owner| Rc::ptr_eq(&owner, realm)));
    if !realm_has_lists {
        realm.session.borrow_mut().require_container_oracle(false);
    }
    Ok(())
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let constructor = ctx.class_constructor::<DomContainerQueryList>();
    let global = ctx.global_object();
    crate::install_interface(ctx, &global, "ContainerQueryList", constructor)
        .map_err(|_| OpError::new("Error", "ContainerQueryList installation failed"))
}
