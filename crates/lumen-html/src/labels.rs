//! Shared label/control association algorithms.
//!
//! The DOM-facing adapter keeps the public `NodeList` wrapper live; these
//! helpers only traverse the current ordinary tree and allocate no temporary
//! collections.

use crate::{Document, Error, NodeId, NodeKind};

/// Whether an HTML element participates in the labelable-element algorithms.
pub fn is_labelable(document: &Document, node: NodeId) -> Result<bool, Error> {
    if document.is_form_associated_custom_element(node) {return Ok(true);}
    Ok(
        match crate::forms::html_element_local_name(document, node) {
            Some("button" | "meter" | "output" | "progress" | "select" | "textarea") => true,
            Some("input") => !document
                .get_attribute_ns_ref(node, None, "type")?
                .is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")),
            _ => false,
        },
    )
}

/// Return the control currently labeled by an HTML `label` element.
///
/// An explicit `for` value selects the first element with that ID in the
/// label's tree, and then tests that element's labelability. If `for` is
/// present but empty or points at a non-labelable first match, descendants do
/// not provide a fallback. Without `for`, the first labelable descendant in
/// tree order is selected.
pub fn label_control(document: &Document, label: NodeId) -> Result<Option<NodeId>, Error> {
    if crate::forms::html_element_local_name(document, label) != Some("label") {
        return Ok(None);
    }

    if let Some(for_value) = document.get_attribute_ns_ref(label, None, "for")? {
        let root = tree_root(document, label)?;
        let first = first_element_with_id(document, root, for_value)?;
        return match first {
            Some(candidate) if is_labelable(document, candidate)? => Ok(Some(candidate)),
            _ => Ok(None),
        };
    }

    let mut current = crate::selector::next_descendant(document, label, label)?;
    while let Some(candidate) = current {
        if is_labelable(document, candidate)? {
            return Ok(Some(candidate));
        }
        current = crate::selector::next_descendant(document, label, candidate)?;
    }
    Ok(None)
}

/// Visit the labels associated with `control` in its current tree order.
///
/// The callback returns `false` to stop early. The control's current tree root
/// is recomputed for each call so a retained adapter `NodeList` follows moves,
/// adoption, and subtree insertion/removal.
pub fn for_each_control_label(
    document: &Document,
    control: NodeId,
    mut visit: impl FnMut(NodeId) -> bool,
) -> Result<(), Error> {
    if !is_labelable(document, control)? {
        return Ok(());
    }
    let root = tree_root(document, control)?;
    let control_id = document.get_attribute_ns_ref(control, None, "id")?;
    let control_is_first_matching_id = match control_id {
        Some(id) => first_element_with_id(document, root, id)? == Some(control),
        None => false,
    };
    let mut current = Some(root);
    while let Some(candidate) = current {
        if crate::forms::html_element_local_name(document, candidate) == Some("label") {
            let label_for = document.get_attribute_ns_ref(candidate, None, "for")?;
            let associated = match label_for {
                Some(value) => control_is_first_matching_id && control_id == Some(value),
                None => label_control(document, candidate)? == Some(control),
            };
            if associated && !visit(candidate) {
                return Ok(());
            }
        }
        current = crate::selector::next_descendant(document, root, candidate)?;
    }
    Ok(())
}

fn tree_root(document: &Document, mut node: NodeId) -> Result<NodeId, Error> {
    while let Some(parent) = document.parent(node)? {
        node = parent;
    }
    Ok(node)
}

fn is_element(document: &Document, node: NodeId) -> Result<bool, Error> {
    Ok(matches!(document.kind(node)?, NodeKind::Element { .. }))
}

fn first_element_with_id(
    document: &Document,
    root: NodeId,
    wanted: &str,
) -> Result<Option<NodeId>, Error> {
    let mut current = Some(root);
    while let Some(candidate) = current {
        if is_element(document, candidate)?
            && document.get_attribute_ns_ref(candidate, None, "id")? == Some(wanted)
        {
            return Ok(Some(candidate));
        }
        current = crate::selector::next_descendant(document, root, candidate)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;
    use alloc::vec::Vec;

    fn element(document: &Document, id: &str) -> NodeId {
        crate::selector::get_element_by_id(document, document.root(), id)
            .unwrap()
            .unwrap()
    }

    fn labels(document: &Document, control: NodeId) -> Vec<NodeId> {
        let mut result = Vec::new();
        for_each_control_label(document, control, |label| {
            result.push(label);
            true
        })
        .unwrap();
        result
    }

    #[test]
    fn label_association_is_tree_scoped_live_and_ordered() {
        let mut document = html::parse(
            "<main><label id='implicit'><span><input id='control'></span></label><label id='explicit' for='control'></label><label id='not-first' for='duplicate'></label><b id='duplicate'></b><input id='duplicate'></main>",
            64,
        )
        .unwrap();
        let control = element(&document, "control");
        let implicit = element(&document, "implicit");
        let explicit = element(&document, "explicit");
        assert_eq!(label_control(&document, implicit), Ok(Some(control)));
        assert_eq!(label_control(&document, explicit), Ok(Some(control)));
        assert_eq!(labels(&document, control), [implicit, explicit]);
        assert_eq!(
            label_control(&document, element(&document, "not-first")),
            Ok(None)
        );

        let explicit = element(&document, "explicit");
        document
            .set_attribute_ns(explicit, None, "for", "elsewhere")
            .unwrap();
        assert_eq!(labels(&document, control), [implicit]);
        document
            .set_attribute_ns(explicit, None, "for", "control")
            .unwrap();
        assert_eq!(labels(&document, control), [implicit, explicit]);
    }

    #[test]
    fn labelability_excludes_hidden_input_and_empty_explicit_for() {
        let mut document = html::parse(
            "<main><label id='empty-for' for=''><input id='inside-empty'></label><label id='empty-match' for=''><input id=''></label><label id='hidden-label' for='hidden'></label><input id='hidden' type='HIDDEN'><output id='output'></output><label id='output-label' for='output'></label></main>",
            64,
        )
        .unwrap();
        let hidden = element(&document, "hidden");
        let output = element(&document, "output");
        assert!(!is_labelable(&document, hidden).unwrap());
        // An empty `for` can still identify an explicit empty `id` elsewhere
        // in the tree. It must not fall back to the label's own descendant.
        let empty_for = element(&document, "empty-for");
        let empty_id_label = element(&document, "empty-match");
        let empty_id_control = label_control(&document, empty_id_label).unwrap().unwrap();
        assert_eq!(document.parent(empty_id_control), Ok(Some(empty_id_label)));
        assert_eq!(
            label_control(&document, empty_for),
            Ok(Some(empty_id_control))
        );
        document
            .set_attribute_ns(empty_id_control, None, "id", "changed")
            .unwrap();
        assert_eq!(label_control(&document, empty_for), Ok(None));
        assert_eq!(label_control(&document, empty_id_label), Ok(None));
        assert_eq!(
            label_control(&document, element(&document, "hidden-label")),
            Ok(None)
        );
        assert_eq!(
            label_control(&document, element(&document, "output-label")),
            Ok(Some(output))
        );
        assert!(labels(&document, hidden).is_empty());
        assert_eq!(
            labels(&document, output),
            [element(&document, "output-label")]
        );
    }
}
