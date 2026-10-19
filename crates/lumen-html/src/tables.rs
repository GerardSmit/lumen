//! HTML table membership and insertion plans over the ordinary document tree.
use crate::{Document, Error, NodeId};
use crate::forms::html_element_local_name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollectionKind { Bodies, TableRows, SectionRows, Cells }

fn children(document: &Document, root: NodeId, mut visit: impl FnMut(NodeId) -> Result<bool, Error>) -> Result<bool, Error> {
    let mut child = document.first_child(root)?;
    while let Some(id) = child {
        if !visit(id)? { return Ok(false); }
        child = document.next_sibling(id)?;
    }
    Ok(true)
}

pub fn visit(document: &Document, root: NodeId, kind: CollectionKind, mut visitor: impl FnMut(NodeId) -> bool) -> Result<(), Error> {
    if kind == CollectionKind::TableRows {
        for phase in 0..3 {
            if !children(document, root, |child| {
                let name = html_element_local_name(document, child);
                if phase == 1 && name == Some("tr") { return Ok(visitor(child)); }
                if matches!((phase, name), (0, Some("thead")) | (1, Some("tbody")) | (2, Some("tfoot"))) {
                    return children(document, child, |row| Ok(html_element_local_name(document, row) != Some("tr") || visitor(row)));
                }
                Ok(true)
            })? { break; }
        }
    } else {
        children(document, root, |child| {
            let name = html_element_local_name(document, child);
            let included = match kind {
                CollectionKind::Bodies => name == Some("tbody"),
                CollectionKind::SectionRows => name == Some("tr"),
                CollectionKind::Cells => matches!(name, Some("td" | "th")),
                CollectionKind::TableRows => false,
            };
            Ok(!included || visitor(child))
        })?;
    }
    Ok(())
}

pub fn count(document: &Document, root: NodeId, kind: CollectionKind) -> Result<usize, Error> {
    let mut count = 0;
    visit(document, root, kind, |_| { count += 1; true })?;
    Ok(count)
}

pub fn item(document: &Document, root: NodeId, kind: CollectionKind, index: usize) -> Result<Option<NodeId>, Error> {
    let mut result = None;
    let mut current = 0;
    visit(document, root, kind, |node| { if current == index { result = Some(node); false } else { current += 1; true } })?;
    Ok(result)
}

pub fn index(document: &Document, root: NodeId, kind: CollectionKind, node: NodeId) -> Result<i32, Error> {
    let mut result = -1;
    let mut current = 0;
    visit(document, root, kind, |id| { if id == node { result = current; false } else { current += 1; true } })?;
    Ok(result)
}

pub fn first_named(document: &Document, root: NodeId, name: &str) -> Result<Option<NodeId>, Error> {
    let mut result = None;
    children(document, root, |id| { if html_element_local_name(document, id) == Some(name) { result = Some(id); Ok(false) } else { Ok(true) } })?;
    Ok(result)
}

pub fn row_index(document: &Document, row: NodeId, section: bool) -> Result<i32, Error> {
    let Some(parent) = document.parent(row)? else { return Ok(-1); };
    match html_element_local_name(document, parent) {
        Some("table") => index(document, parent, CollectionKind::TableRows, row),
        Some("thead" | "tbody" | "tfoot") if section => index(document, parent, CollectionKind::SectionRows, row),
        Some("thead" | "tbody" | "tfoot") => match document.parent(parent)? {
            Some(table) if html_element_local_name(document, table) == Some("table") => index(document, table, CollectionKind::TableRows, row),
            _ => Ok(-1),
        },
        _ => Ok(-1),
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Insertion { pub parent: NodeId, pub before: Option<NodeId>, pub create_body: bool }

pub fn insertion(document: &Document, root: NodeId, kind: CollectionKind, requested: i32) -> Result<Insertion, Error> {
    let length = count(document, root, kind)?;
    if requested < -1 || (requested >= 0 && requested as usize > length) { return Err(Error::IndexSize); }
    let index = if requested == -1 { length } else { requested as usize };
    if kind == CollectionKind::TableRows {
        if length == 0 {
            let bodies = count(document, root, CollectionKind::Bodies)?;
            return Ok(Insertion { parent: item(document, root, CollectionKind::Bodies, bodies.saturating_sub(1))?.unwrap_or(root), before: None, create_body: bodies == 0 });
        }
        let row = item(document, root, kind, index.min(length - 1))?.ok_or(Error::InvalidNode)?;
        return Ok(Insertion { parent: document.parent(row)?.ok_or(Error::InvalidNode)?, before: (index < length).then_some(row), create_body: false });
    }
    Ok(Insertion { parent: root, before: item(document, root, kind, index)?, create_body: false })
}

pub fn deletion(document: &Document, root: NodeId, kind: CollectionKind, requested: i32) -> Result<Option<NodeId>, Error> {
    let length = count(document, root, kind)?;
    if requested == -1 { return item(document, root, kind, length.saturating_sub(1)); }
    if requested < 0 || requested as usize >= length { return Err(Error::IndexSize); }
    item(document, root, kind, requested as usize)
}

pub fn reflected_span(document: &Document, node: NodeId, attribute: &str, minimum: u32, maximum: u32) -> Result<u32, Error> {
    Ok(crate::forms::reflected_clamped_unsigned_long(
        document.get_attribute_ns_ref(node, None, attribute)?, minimum, maximum, 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{vec, vec::Vec};
    use crate::{Namespace, NodeKind};
    fn element(document: &mut Document, namespace: Namespace, name: &str) -> NodeId {
        document.create(NodeKind::Element { namespace, name: name.into(), attributes: Vec::new() }).unwrap()
    }

    #[test]
    fn specification_window_table_order_is_namespace_qualified_and_insertion_uses_real_parent() {
        let mut document = Document::new(32);
        let table = element(&mut document, Namespace::Html, "table");
        let footer = element(&mut document, Namespace::Html, "tfoot");
        let body = element(&mut document, Namespace::Html, "tbody");
        let header = element(&mut document, Namespace::Html, "thead");
        for section in [footer, body, header] { document.append(table, section).unwrap(); }
        let mut rows = Vec::new();
        for section in [footer, body, header] {
            let row = element(&mut document, Namespace::Html, "tr");
            document.append(section, row).unwrap(); rows.push(row);
        }
        let foreign = element(&mut document, Namespace::Svg, "tr");
        document.append(body, foreign).unwrap();
        let mut ordered = Vec::new();
        visit(&document, table, CollectionKind::TableRows, |node| { ordered.push(node); true }).unwrap();
        assert_eq!(ordered, vec![rows[2], rows[1], rows[0]]);
        let append = insertion(&document, table, CollectionKind::TableRows, -1).unwrap();
        assert_eq!(append.parent, footer);assert_eq!(append.before, None);assert!(!append.create_body);
        let before = insertion(&document, table, CollectionKind::TableRows, 1).unwrap();
        assert_eq!(before.parent, body);assert_eq!(before.before, Some(rows[1]));
        assert_eq!(row_index(&document, rows[0], false).unwrap(), 2);
        assert_eq!(row_index(&document, rows[0], true).unwrap(), 0);
    }

    #[test]
    fn specification_window_table_plan_admission_and_empty_deletion_are_atomic() {
        let mut document = Document::new(3);
        let table = element(&mut document, Namespace::Html, "table");
        let plan = insertion(&document, table, CollectionKind::TableRows, -1).unwrap();
        assert!(plan.create_body);
        assert_eq!(document.ensure_clone_capacity(2), Err(Error::LimitExceeded));
        assert_eq!(count(&document, table, CollectionKind::Bodies).unwrap(), 0);
        assert_eq!(count(&document, table, CollectionKind::TableRows).unwrap(), 0);
        assert!(matches!(insertion(&document, table, CollectionKind::TableRows, 1), Err(Error::IndexSize)));
        assert_eq!(deletion(&document, table, CollectionKind::TableRows, -1).unwrap(), None);
        assert_eq!(deletion(&document, table, CollectionKind::TableRows, 0), Err(Error::IndexSize));
    }
}
