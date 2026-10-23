//! `<selectedcontent>` synchronisation for customizable selects.
//!
//! A `selectedcontent` descendant of a select mirrors the children of the
//! select's selected option. Selectedness stays adapter-owned, so callers pass
//! it in the same shape the shared selected-option walk expects.

use crate::{equality, forms, selector, Document, Error, Namespace, NodeId, NodeKind};
use alloc::vec::Vec;

fn is_html_named(document: &Document, node: NodeId, tag: &str) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == tag
    )
}

fn children(document: &Document, parent: NodeId) -> Result<Vec<NodeId>, Error> {
    let mut list = Vec::new();
    let mut child = document.first_child(parent)?;
    while let Some(id) = child {
        list.push(id);
        child = document.next_sibling(id)?;
    }
    Ok(list)
}

/// The `selectedcontent` elements whose nearest select ancestor is `select`.
pub fn selected_content_targets(document: &Document, select: NodeId) -> Result<Vec<NodeId>, Error> {
    let mut targets = Vec::new();
    let mut current = selector::next_descendant(document, select, select)?;
    while let Some(node) = current {
        if is_html_named(document, node, "select") {
            current = selector::next_after_subtree(document, select, node)?;
            continue;
        }
        if is_html_named(document, node, "selectedcontent") {
            targets.push(node);
        }
        current = selector::next_descendant(document, select, node)?;
    }
    Ok(targets)
}

/// The select that owns `selectedcontent`, if its nearest select ancestor exists.
pub fn selected_content_owner(document: &Document, node: NodeId) -> Result<Option<NodeId>, Error> {
    match document.parent(node)? {
        Some(parent) => forms::select_ancestor(document, parent),
        None => Ok(None),
    }
}

/// Every select that should be refreshed after `root` entered the tree: selects
/// inside it and the owners of any `selectedcontent` it contains.
pub fn selects_for_inserted_subtree(document: &Document, root: NodeId) -> Result<Vec<NodeId>, Error> {
    let mut selects = Vec::new();
    fn push(select: NodeId, selects: &mut Vec<NodeId>) {
        if !selects.contains(&select) {
            selects.push(select);
        }
    }
    let mut current = Some(root);
    while let Some(node) = current {
        if is_html_named(document, node, "select") {
            push(node, &mut selects);
        } else if is_html_named(document, node, "selectedcontent") {
            if let Some(owner) = selected_content_owner(document, node)? {
                push(owner, &mut selects);
            }
        }
        current = selector::next_descendant(document, root, node)?;
    }
    Ok(selects)
}

/// Clone the selected option's children into `only` (or every owned
/// `selectedcontent`) when their content differs. Returns the replaced old
/// children so the caller can release them. A select with `multiple` is left
/// untouched.
pub fn sync_selected_content(
    document: &mut Document,
    select: NodeId,
    mut selectedness: impl FnMut(NodeId) -> Option<bool>,
    only: Option<&[NodeId]>,
) -> Result<Vec<NodeId>, Error> {
    let mut removed = Vec::new();
    if !document.selected_content_created() {
        return Ok(removed);
    }
    if !is_html_named(document, select, "select")
        || document.get_attribute_ns_ref(select, None, "multiple")?.is_some()
    {
        return Ok(removed);
    }
    let targets = match only {
        Some(list) => list.to_vec(),
        None => selected_content_targets(document, select)?,
    };
    if targets.is_empty() {
        return Ok(removed);
    }
    let mut source = None;
    forms::for_each_selected_option_by(document, select, &mut selectedness, |option, _, _| {
        source = Some(option);
        false
    })?;
    let source_children = match source {
        Some(option) => children(document, option)?,
        None => Vec::new(),
    };
    for target in targets {
        let current = children(document, target)?;
        let mut same = current.len() == source_children.len();
        if same {
            for (existing, wanted) in current.iter().zip(source_children.iter()) {
                if !equality::is_equal_node(document, *existing, document, *wanted)? {
                    same = false;
                    break;
                }
            }
        }
        if same {
            continue;
        }
        let mut clones = Vec::with_capacity(source_children.len());
        for child in &source_children {
            clones.push(document.clone_subtree(*child)?);
        }
        document.replace_children_many(target, &clones)?;
        removed.extend(current);
    }
    Ok(removed)
}
