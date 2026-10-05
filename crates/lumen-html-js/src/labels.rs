//! Typed adapter helpers for HTML label/control association.
//!
//! `DomNodeList` owns the live collection behavior. These functions only
//! implement the IDL value conversion and reuse the per-node weak collection
//! cache already used by other SameObject collections.

use super::{DomRealm, collections::DomNodeList};
use lumen::embed::{Ctx, OpResult, Value, WeakValue};
use lumen_html::{NodeId, labels as core_labels};
use std::{cell::RefCell, collections::HashMap, rc::Rc};

/// Return the current control for an `HTMLLabelElement.control` getter.
pub(crate) fn label_control(ctx: &mut Ctx, realm: &Rc<DomRealm>, label: NodeId) -> OpResult<Value> {
    let control = {
        let session = realm.session.borrow();
        core_labels::label_control(session.document(), label).map_err(super::dom_error)?
    };
    Ok(realm.wrap_option(ctx, control))
}

/// Return `HTMLLabelElement.form`, which follows the label's control rather
/// than the label element's own ancestors.
pub(crate) fn label_form(ctx: &mut Ctx, realm: &Rc<DomRealm>, label: NodeId) -> OpResult<Value> {
    let form = {
        let session = realm.session.borrow();
        let document = session.document();
        core_labels::label_control(document, label)
            .map_err(super::dom_error)?
            .and_then(|control| lumen_html::forms::form_owner(document, control))
    };
    Ok(realm.wrap_option(ctx, form))
}

/// Return the live, SameObject `labels` NodeList for a labelable control.
///
/// Hidden inputs expose `null`; a previously obtained NodeList remains in the
/// normal weak identity cache, so changing the input back to a labelable type
/// returns the same live collection.
pub(crate) fn control_labels(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    control: NodeId,
    owner: Value,
    collections: &RefCell<HashMap<String, WeakValue>>,
) -> OpResult<Value> {
    let labelable = {
        let session = realm.session.borrow();
        core_labels::is_labelable(session.document(), control).map_err(super::dom_error)?
    };
    if !labelable {
        return Ok(Value::Null);
    }
    if let Some(value) = collections
        .borrow()
        .get("labels")
        .and_then(WeakValue::upgrade)
    {
        return Ok(value);
    }

    // The collection implementation is intentionally shared with child lists,
    // selected options, and form controls. Its LabelAssociations source stores
    // only the control NodeId and recomputes the current root and matches.
    let collection = ctx.new_instance(DomNodeList::label_associations(
        realm.clone(),
        control,
        owner,
    ));
    collections.borrow_mut().insert(
        "labels".into(),
        ctx.weak_value(&collection).expect("labels collection"),
    );
    Ok(collection)
}

#[cfg(test)]
mod tests {
    use lumen::Engine;
    use lumen::embed::Value;

    fn eval(engine: &mut Engine, source: &str) -> String {
        let result = engine.eval_value(source).expect("script parses");
        match result {
            Ok(Value::Str(value)) => value.to_string(),
            Ok(_) => panic!("expected diagnostic string"),
            Err(error) => {
                let message = engine
                    .ctx()
                    .coerce_string(&error)
                    .map(|message| message.to_string())
                    .unwrap_or_else(|_| "unknown exception".into());
                panic!("label contract threw: {message}");
            }
        }
    }

    #[test]
    fn label_control_and_labels_node_lists_are_live_same_object() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form id='owner'><main><label id='implicit'><span><input id='control'></span></label><label id='explicit' for='control'></label><label id='hidden-label' for='hidden'></label><input id='hidden' type='hidden'></main></form>",
            128,
        )
        .unwrap();
        let failures = eval(
            &mut engine,
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const control = document.getElementById('control');
                const implicit = document.getElementById('implicit');
                const explicit = document.getElementById('explicit');
                const form = document.getElementById('owner');
                const labels = control.labels;
                check(labels instanceof NodeList, 'node-list-brand');
                check(labels === control.labels, 'same-object');
                check(labels.length === 2 && labels[0] === implicit && labels[1] === explicit, 'tree-order');
                check(implicit.control === control && explicit.control === control &&
                    implicit.form === form && explicit.form === form, 'label-control-form');
                explicit.htmlFor = 'elsewhere';
                check(labels.length === 1 && labels[0] === implicit && explicit.control === null, 'for-mutation');
                explicit.htmlFor = 'control';
                explicit.remove();
                check(labels.length === 1 && labels[0] === implicit, 'label-removal');
                const hidden = document.getElementById('hidden');
                check(hidden.labels === null, 'hidden-null');
                hidden.type = 'text';
                const hiddenLabels = hidden.labels;
                check(hiddenLabels.length === 1 && hiddenLabels[0].id === 'hidden-label', 'hidden-becomes-labelable');
                hidden.type = 'hidden';
                check(hidden.labels === null && hiddenLabels.length === 0, 'retained-list-after-hidden');
                hidden.type = 'text';
                check(hidden.labels === hiddenLabels && hiddenLabels.length === 1, 'retained-list-after-visible-again');
                return failures.join(',');
            })()"#,
        );
        assert!(failures.is_empty(), "{failures}");
    }
}
