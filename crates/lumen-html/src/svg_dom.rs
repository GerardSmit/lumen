//! SVG DOM identities and ancestry, independent of native wrapper or rendering state.
use crate::{Document, Error, Namespace, NodeId, NodeKind};

pub fn is_svg_root(document: &Document, node: NodeId) -> bool {
    matches!(document.kind(node), Ok(NodeKind::Element { namespace: Namespace::Svg, .. }))
        && document.element_name_parts(node).is_ok_and(|(_, local)| local == "svg")
}

/// SVG's owner excludes the element itself and follows ordinary DOM parents,
/// rather than crossing a shadow boundary or relying on document ownership.
pub fn owner_svg_element(document: &Document, node: NodeId) -> Result<Option<NodeId>, Error> {
    let mut parent = document.parent(node)?;
    while let Some(id) = parent {
        if is_svg_root(document, id) {
            return Ok(Some(id));
        }
        parent = document.parent(id)?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn svg(document: &mut Document, name: &str) -> NodeId {
        document.create(NodeKind::Element {
            namespace: Namespace::Svg, name: name.into(), attributes: Vec::new(),
        }).unwrap()
    }

    #[test]
    fn svg_ancestry_uses_actual_names_excludes_self_and_reclaims_bounded_trees() {
        let mut document = Document::new(32);
        for _ in 0..100 {
            let root = svg(&mut document, "p:svg");
            document.append(document.root(), root).unwrap();
            assert!(is_svg_root(&document, root));
            assert_eq!(owner_svg_element(&document, root).unwrap(), None);
            let literal = document.create_unprefixed_element(Namespace::Svg, "p:svg".into(), Vec::new()).unwrap();
            document.append(root, literal).unwrap();
            assert!(!is_svg_root(&document, literal));
            let mut parent = literal;
            for _ in 0..16 {
                let child = svg(&mut document, "g");
                document.append(parent, child).unwrap();
                assert_eq!(owner_svg_element(&document, child).unwrap(), Some(root));
                parent = child;
            }
            let nested = svg(&mut document, "svg");
            document.append(parent, nested).unwrap();
            assert_eq!(owner_svg_element(&document, nested).unwrap(), Some(root));
            let leaf = svg(&mut document, "rect");
            document.append(nested, leaf).unwrap();
            assert_eq!(owner_svg_element(&document, leaf).unwrap(), Some(nested));
            document.remove(nested).unwrap();
            assert_eq!(owner_svg_element(&document, nested).unwrap(), None);
            assert_eq!(owner_svg_element(&document, leaf).unwrap(), Some(nested));
            document.destroy_subtree(nested).unwrap();
            document.remove(root).unwrap();
            document.destroy_subtree(root).unwrap();
            assert_eq!(document.node_count(), 1);
            assert_eq!(document.literal_colon_names.capacity(), 0);
        }
    }
}
