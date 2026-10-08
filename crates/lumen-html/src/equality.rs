//! DOM structural equality, independent of document ownership and host wrappers.
use crate::{Document, Error, Namespace, NodeId, NodeKind};
use alloc::vec::Vec;

fn namespace(namespace: &Namespace) -> &str {
    match namespace {
        Namespace::Html => "http://www.w3.org/1999/xhtml",
        Namespace::Svg => "http://www.w3.org/2000/svg",
        Namespace::MathMl => "http://www.w3.org/1998/Math/MathML",
        Namespace::Other(uri) => uri,
    }
}

fn local(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

fn attribute_local<'a>(name: &'a str, namespace: Option<&str>) -> &'a str {
    if namespace.is_some_and(|uri| !uri.is_empty()) { local(name) } else { name }
}

fn attributes_equal(left: &Document, a: NodeId, right: &Document, b: NodeId) -> Result<bool, Error> {
    let (NodeKind::Element { attributes: aa, .. }, NodeKind::Element { attributes: ba, .. }) =
        (left.kind(a)?, right.kind(b)?) else { return Err(Error::WrongKind); };
    if aa.len() != ba.len() { return Ok(false); }
    if aa.len() <= 16 {
        for (index, (name, value)) in aa.iter().enumerate() {
            let uri = left.attribute_namespace_uri_at(a, index);
            if right.get_attribute_ns_ref(b, uri, attribute_local(name, uri))?
                != Some(value.as_str()) { return Ok(false); }
        }
        return Ok(true);
    }
    // Large attribute maps use borrowed sortable keys rather than quadratic lookup or Attr
    // materialization. Only two bounded temporary arrays are allocated; no strings are copied.
    fn keys(document: &Document, id: NodeId) -> Result<Vec<(&str, &str, &str)>, Error> {
        let NodeKind::Element { attributes, .. } = document.kind(id)? else { return Err(Error::WrongKind); };
        let mut keys = Vec::new();
        keys.try_reserve_exact(attributes.len()).map_err(|_| Error::LimitExceeded)?;
        for (name, value) in attributes { keys.push(("", name.as_str(), value.as_str())); }
        if let Some((_, namespaces)) = document.attribute_namespaces.iter().find(|(owner, _)| *owner == id) {
            for (index, uri) in namespaces {
                keys[*index].0 = uri.as_ref();
                keys[*index].1 = attribute_local(keys[*index].1, Some(uri));
            }
        }
        keys.sort_unstable();
        Ok(keys)
    }
    Ok(keys(left, a)? == keys(right, b)?)
}

fn data_equal(left: &Document, a: NodeId, right: &Document, b: NodeId) -> Result<bool, Error> {
    Ok(match (left.kind(a)?, right.kind(b)?) {
        (NodeKind::Document, NodeKind::Document) | (NodeKind::DocumentFragment, NodeKind::DocumentFragment) => true,
        (NodeKind::DocumentType(an), NodeKind::DocumentType(bn)) => an == bn &&
            left.doctype_public_id(a)? == right.doctype_public_id(b)? &&
            left.doctype_system_id(a)? == right.doctype_system_id(b)?,
        (NodeKind::Element { namespace: an, .. }, NodeKind::Element { namespace: bn, .. }) =>
            namespace(an) == namespace(bn) && left.element_name_parts(a)? == right.element_name_parts(b)? &&
            attributes_equal(left, a, right, b)?,
        (NodeKind::Attribute { namespace_uri: an, qualified_name: aq, value: av, .. },
         NodeKind::Attribute { namespace_uri: bn, qualified_name: bq, value: bv, .. }) =>
            an.as_deref().map_or("", |uri| uri.as_ref()) == bn.as_deref().map_or("", |uri| uri.as_ref()) &&
            attribute_local(aq, an.as_deref().map(|uri| uri.as_ref())) ==
                attribute_local(bq, bn.as_deref().map(|uri| uri.as_ref())) && av == bv,
        (NodeKind::Text(a), NodeKind::Text(b)) | (NodeKind::CData(a), NodeKind::CData(b)) |
        (NodeKind::Comment(a), NodeKind::Comment(b)) => a == b,
        (NodeKind::ProcessingInstruction { target: at, data: ad },
         NodeKind::ProcessingInstruction { target: bt, data: bd }) => at == bt && ad == bd,
        _ => false,
    })
}

/// Compare the ordinary child trees, with ordered children and unordered expanded-name
/// attributes. Shadow trees, template contents, owner documents and mutable state are not
/// ordinary children. Parent links make traversal iterative with constant tree scratch space.
pub fn is_equal_node(left: &Document, a: NodeId, right: &Document, b: NodeId) -> Result<bool, Error> {
    if core::ptr::eq(left, right) && a == b {
        left.kind(a)?;
        return Ok(true);
    }
    let (mut current_a, mut current_b) = (a, b);
    loop {
        if !data_equal(left, current_a, right, current_b)? { return Ok(false); }
        match (left.first_child(current_a)?, right.first_child(current_b)?) {
            (Some(ac), Some(bc)) => { current_a = ac; current_b = bc; continue; }
            (None, None) => {},
            _ => return Ok(false),
        }
        loop {
            if current_a == a || current_b == b { return Ok(current_a == a && current_b == b); }
            match (left.next_sibling(current_a)?, right.next_sibling(current_b)?) {
                (Some(an), Some(bn)) => { current_a = an; current_b = bn; break; }
                (None, None) => {
                    current_a = left.parent(current_a)?.ok_or(Error::Hierarchy)?;
                    current_b = right.parent(current_b)?.ok_or(Error::Hierarchy)?;
                },
                _ => return Ok(false),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, string::String};

    #[test]
    fn large_unordered_attributes_compare_without_materializing_attr_nodes() {
        let mut a = Document::new(8);
        let mut b = Document::new(8);
        let ar = a.create(NodeKind::Element { namespace: Namespace::Html, name: "div".into(), attributes: Vec::new() }).unwrap();
        let br = b.create(NodeKind::Element { namespace: Namespace::Html, name: "div".into(), attributes: Vec::new() }).unwrap();
        for index in 0..64 { a.set_attribute_ns(ar, Some("urn:attrs"), &format!("a:k{index}"), &format!("v{index}")).unwrap(); }
        for index in (0..64).rev() { b.set_attribute_ns(br, Some("urn:attrs"), &format!("b:k{index}"), &format!("v{index}")).unwrap(); }
        let counts = (a.node_count(), b.node_count());
        for _ in 0..100 { assert!(is_equal_node(&a, ar, &b, br).unwrap()); }
        assert_eq!((a.node_count(), b.node_count()), counts);
        assert!(a.materialized_attribute_nodes(ar).is_none());
        assert!(b.materialized_attribute_nodes(br).is_none());
        b.set_attribute_ns(br, Some("urn:attrs"), "b:k32", "changed").unwrap();
        assert!(!is_equal_node(&a, ar, &b, br).unwrap());
    }

    #[test]
    fn deep_comparison_preserves_hierarchy_without_recursive_stack() {
        let mut a = Document::new(4098);
        let mut b = Document::new(4098);
        let (ar, br) = (a.root(), b.root());
        let (mut ac, mut bc) = (ar, br);
        for _ in 0..4096 {
            let an = a.create(NodeKind::Element { namespace: Namespace::Html, name: "i".into(), attributes: Vec::new() }).unwrap();
            let bn = b.create(NodeKind::Element { namespace: Namespace::Html, name: "i".into(), attributes: Vec::new() }).unwrap();
            a.append(ac, an).unwrap(); b.append(bc, bn).unwrap();
            ac = an; bc = bn;
        }
        assert!(is_equal_node(&a, ar, &b, br).unwrap());
        let text = b.create(NodeKind::Text(String::from("different"))).unwrap();
        b.append(bc, text).unwrap();
        assert!(!is_equal_node(&a, ar, &b, br).unwrap());
        let parent = b.parent(bc).unwrap().unwrap();
        b.append(parent, text).unwrap();
        assert!(!is_equal_node(&a, ar, &b, br).unwrap());
    }
}
