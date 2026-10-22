//! Named properties of `HTMLDocument`: `embed`, `form`, `iframe`, `img` and `object` elements
//! reachable as `document[name]`.
use super::*;
use crate::collections::{DescendantFilter, DomHtmlCollection, DomNodeList};

fn legacy_name_tag(tag: &str) -> bool {
    matches!(tag, "embed" | "form" | "iframe" | "img" | "object")
}

/// The names under which `id` is exposed, in the order the supported property names list them.
fn exposed_names(document: &lumen_html::Document, id: NodeId) -> Result<Vec<String>, Error> {
    let NodeKind::Element { namespace: Namespace::Html, name, .. } = document.kind(id)? else {
        return Ok(Vec::new());
    };
    if !legacy_name_tag(name.as_str()) {
        return Ok(Vec::new());
    }
    let attribute = |attribute: &str| -> Result<Option<String>, Error> {
        Ok(document.get_attribute_ns_ref(id, None, attribute)?.filter(|value| !value.is_empty()).map(str::to_owned))
    };
    let element_name = attribute("name")?;
    let mut names: Vec<String> = element_name.iter().cloned().collect();
    let element_id = match name.as_str() {
        "object" => attribute("id")?,
        "img" if element_name.is_some() => attribute("id")?,
        _ => None,
    };
    names.extend(element_id);
    Ok(names)
}

pub(crate) fn matches(document: &lumen_html::Document, id: NodeId, wanted: &str) -> Result<bool, Error> {
    Ok(!wanted.is_empty() && exposed_names(document, id)?.iter().any(|name| name == wanted))
}

fn nodes_named(realm: &DomRealm, root: NodeId, wanted: &str) -> OpResult<Vec<NodeId>> {
    let session = realm.session.borrow();
    let document = session.document();
    let mut found = Vec::new();
    let mut next = next_descendant(document, root, root).map_err(dom_error)?;
    while let Some(node) = next {
        if matches(document, node, wanted).map_err(dom_error)? {
            found.push(node);
        }
        next = next_descendant(document, root, node).map_err(dom_error)?;
    }
    Ok(found)
}

pub(crate) fn supported(realm: &DomRealm, root: NodeId, wanted: &str) -> OpResult<bool> {
    Ok(!nodes_named(realm, root, wanted)?.is_empty())
}

pub(crate) fn names(realm: &DomRealm, root: NodeId) -> OpResult<Vec<String>> {
    let session = realm.session.borrow();
    let document = session.document();
    let mut seen = std::collections::HashSet::new();
    let mut names = Vec::new();
    let mut next = next_descendant(document, root, root).map_err(dom_error)?;
    while let Some(node) = next {
        for name in exposed_names(document, node).map_err(dom_error)? {
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        next = next_descendant(document, root, node).map_err(dom_error)?;
    }
    Ok(names)
}

pub(crate) fn value(ctx: &mut Ctx, realm: &Rc<DomRealm>, root: NodeId, owner: Value, wanted: &str) -> OpResult<Value> {
    let nodes = nodes_named(realm, root, wanted)?;
    match nodes.as_slice() {
        [] => Ok(Value::Undefined),
        [node] => {
            let element = realm.wrap(ctx, *node);
            let is_iframe = realm.session.borrow().document().kind(*node).map_or(false, |kind| {
                matches!(kind, NodeKind::Element { name, .. } if name.as_str() == "iframe")
            });
            if is_iframe {
                if let Some(frame) = browsing_context::ensure_frame_for_node(ctx, realm, *node)? {
                    if let Some(window) = frame.window_proxy() {
                        return Ok(window);
                    }
                }
            }
            Ok(element)
        }
        _ => Ok(DomHtmlCollection::create(
            ctx,
            DomNodeList::descendants(realm.clone(), root, DescendantFilter::DocumentNamed(wanted.to_owned()), owner),
        )),
    }
}
