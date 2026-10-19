//! HTML focusability and scope ordering, independent of a script engine.
use crate::{Document, Error, Namespace, NodeId, NodeKind};
use alloc::{collections::BTreeMap, vec::Vec};
use core::cmp::Ordering;

pub fn tabindex(document: &Document, node: NodeId) -> Option<&str> {
    document.get_attribute_ns_ref(node, None, "tabindex").ok().flatten()
        .filter(|raw| lumen_common::html_numbers::parse_integer_sign(raw).is_some())
}

pub fn first_summary(document: &Document, node: NodeId) -> bool {
    if crate::forms::html_element_local_name(document, node) != Some("summary") { return false; }
    let Some(parent) = document.parent(node).ok().flatten() else { return false; };
    if crate::forms::html_element_local_name(document, parent) != Some("details") { return false; }
    let mut child = document.first_child(parent).ok().flatten();
    while let Some(candidate) = child {
        if crate::forms::html_element_local_name(document, candidate) == Some("summary") { return candidate == node; }
        child = document.next_sibling(candidate).ok().flatten();
    }
    false
}

/// Intrinsic focusability before rendering/inertness and connection checks.
pub fn focusable(document: &Document, node: NodeId) -> bool {
    if !matches!(document.kind(node), Ok(NodeKind::Element { .. }))
        || crate::forms::selector_disabled_state(document, node) == Some(true) { return false; }
    if document.shadow_root(node).ok().flatten().and_then(|root|document.shadow_options(root).ok().flatten())
        .is_some_and(|options|options.delegates_focus) { return false; }
    if crate::forms::html_element_local_name(document,node) == Some("input")
        && crate::forms::input_type_state(document,node).eq_ignore_ascii_case("hidden") { return false; }
    if tabindex(document,node).is_some() { return true; }
    if matches!(document.kind(node),Ok(NodeKind::Element {namespace:Namespace::Svg,..}))
        && document.element_name_parts(node).is_ok_and(|(_,local)|local=="a") {
        return document.get_attribute_ns_ref(node,None,"href").ok().flatten().is_some()
            || document.get_attribute_ns_ref(node,Some("http://www.w3.org/1999/xlink"),"href").ok().flatten().is_some();
    }
    match crate::forms::html_element_local_name(document,node) {
        Some("input" | "button" | "textarea" | "select" | "iframe" | "frame" | "object") => true,
        Some("summary") => first_summary(document,node),
        Some("a" | "area") => document.get_attribute_ns_ref(node,None,"href").ok().flatten().is_some(),
        Some("dialog") => document.get_attribute_ns_ref(node,None,"open").ok().flatten().is_some(),
        _ => crate::element_metadata::is_editing_host(document,node),
    }
}

pub fn idl_tabindex(document: &Document, node: NodeId) -> i32 {
    if let Some(value) = document.get_attribute_ns_ref(node,None,"tabindex").ok().flatten()
        .and_then(lumen_common::html_numbers::parse_integer_i32) { return value; }
    let default = match document.kind(node) {
        Ok(NodeKind::Element { namespace: Namespace::Html, .. }) => matches!(document.element_name_parts(node).ok().map(|(_,local)|local).unwrap_or(""),
            "a" | "area" | "button" | "frame" | "iframe" | "input" | "object" | "select" | "textarea") || first_summary(document,node),
        Ok(NodeKind::Element { namespace: Namespace::Svg | Namespace::MathMl, .. }) => document.element_name_parts(node).is_ok_and(|(_,local)|local=="a"),
        _ => false,
    };
    if default { 0 } else { -1 }
}

fn positive_digits(raw: &str) -> Option<&str> {
    if lumen_common::html_numbers::parse_integer_sign(raw) != Some(Ordering::Greater) { return None; }
    let raw = raw.trim_start_matches(|c| matches!(c,'\t'|'\n'|'\x0c'|'\r'|' ')).trim_start_matches('+');
    let end = raw.bytes().take_while(u8::is_ascii_digit).count();
    Some(raw[..end].trim_start_matches('0'))
}

/// Compare mathematical tabindex values without narrowing arbitrarily long
/// HTML digit prefixes. Stable sorting preserves shadow-including tree ties.
fn compare(document: &Document, a: NodeId, b: NodeId) -> Ordering {
    let a = tabindex(document,a).and_then(positive_digits);
    let b = tabindex(document,b).and_then(positive_digits);
    match (a,b) {
        (Some(a),Some(b)) => a.len().cmp(&b.len()).then_with(||a.cmp(b)),
        (Some(_),None) => Ordering::Less,
        (None,Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

fn scope_owner(document: &Document, mut node: NodeId, trigger: &impl Fn(NodeId)->Option<NodeId>) -> Result<Option<NodeId>,Error> {
    loop {
        let Some(parent) = document.parent(node)? else { return Ok(None); };
        if document.shadow_root(parent)?.is_some() { return document.assigned_slot(node); }
        if let Some(host) = document.shadow_host(parent)? { return Ok(Some(host)); }
        if document.parent(parent)? == Some(document.root()) { return Ok(Some(document.root())); }
        if let Some(trigger) = trigger(node) { return Ok(Some(trigger)); }
        node = parent;
    }
}

/// Flatten each distinct tabindex scope after sorting locally. The explicit
/// work stack avoids native recursion for deeply nested authored shadow trees.
pub fn sequential_order(document: &Document, rendered: impl Fn(NodeId)->bool,
    trigger: impl Fn(NodeId)->Option<NodeId>) -> Result<Vec<NodeId>,Error> {
    let root = document.root();
    let mut scopes: BTreeMap<u128,Vec<NodeId>> = BTreeMap::new();
    let mut owners = Vec::new();
    let mut cursor = Some(root);
    while let Some(node) = cursor {
        if matches!(document.kind(node)?,NodeKind::Element { .. }) {
            let owner = document.shadow_root(node)?.is_some()
                || crate::forms::html_element_local_name(document,node) == Some("slot")
                || document.top_layer_entries().any(|(popover,_,_)| trigger(popover) == Some(node));
            if owner { owners.try_reserve(1).map_err(|_|Error::LimitExceeded)?; owners.push(node); }
            let negative = tabindex(document,node).is_some_and(|raw|
                lumen_common::html_numbers::parse_integer_sign(raw) == Some(Ordering::Less));
            if !negative && (owner || rendered(node)) {
                if let Some(scope) = scope_owner(document,node,&trigger)? {
                    let items = scopes.entry(scope.key()).or_default();
                    items.try_reserve(1).map_err(|_|Error::LimitExceeded)?;
                    items.push(node);
                }
            }
        }
        cursor = crate::selector::next_shadow_including_descendant(document,root,node)?;
    }
    for items in scopes.values_mut() { items.sort_by(|a,b|compare(document,*a,*b)); }
    owners.sort_unstable_by_key(|node|node.key());
    let mut pending = scopes.remove(&root.key()).unwrap_or_default();
    pending.reverse();
    let mut result = Vec::new();
    while let Some(node) = pending.pop() {
        if rendered(node) {
            // A delegates-focus host contributes its scope rather than a
            // second stop at the host itself during sequential navigation.
            let delegates = document.shadow_root(node)?.and_then(|root|document.shadow_options(root).ok().flatten())
                .is_some_and(|options|options.delegates_focus);
            if !delegates { result.try_reserve(1).map_err(|_|Error::LimitExceeded)?; result.push(node); }
        }
        if owners.binary_search_by_key(&node.key(),|node|node.key()).is_ok() {
            if let Some(items) = scopes.remove(&node.key()) {
                pending.try_reserve(items.len()).map_err(|_|Error::LimitExceeded)?;
                pending.extend(items.into_iter().rev());
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_window_focus_scopes_keep_local_tabindex_and_namespace_disabled_rules() {
        let mut document=crate::html::parse("<!doctype html><button id=before tabindex=1></button><div id=host tabindex=0></div><button id=after tabindex=3></button><div id=plain tabindex=invalid disabled></div><details><summary id=first></summary><summary id=second></summary></details>",128).unwrap();
        let find=|document:&Document,id:&str|crate::selector::get_element_by_id(document,document.root(),id).unwrap().unwrap();
        let host=find(&document,"host");
        let shadow=document.attach_shadow(host,crate::ShadowMode::Open).unwrap();
        let late=document.create_unprefixed_element(Namespace::Html,"button".into(),Vec::new()).unwrap();
        document.set_attribute(late,"tabindex","2").unwrap();
        document.append(shadow,late).unwrap();
        let early=document.create_unprefixed_element(Namespace::Html,"button".into(),Vec::new()).unwrap();
        document.set_attribute(early,"tabindex"," +0001trailing").unwrap();
        document.append(shadow,early).unwrap();
        let order=sequential_order(&document,|node|focusable(&document,node),|_|None).unwrap();
        assert_eq!(order,alloc::vec![find(&document,"before"),find(&document,"after"),host,early,late,find(&document,"first")]);
        let plain=find(&document,"plain");
        assert!(!focusable(&document,plain));
        document.set_attribute(plain,"tabindex"," -2147483648tail").unwrap();
        assert!(focusable(&document,plain),"ordinary disabled attribute does not disable an arbitrary element");
        assert_eq!(idl_tabindex(&document,plain),i32::MIN);
        document.set_attribute(plain,"tabindex","2147483648").unwrap();
        assert_eq!(idl_tabindex(&document,plain),-1,"IDL long overflow uses the historical element fallback");
        assert!(!focusable(&document,find(&document,"second")));
        document.set_attribute(early,"tabindex",&"9".repeat(80)).unwrap();
        let order=sequential_order(&document,|node|focusable(&document,node),|_|None).unwrap();
        assert!(order.iter().position(|node|*node==late).unwrap()<order.iter().position(|node|*node==early).unwrap());
    }
}
