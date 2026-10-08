//! Shared DOM boundary-point ordering and CharacterData tree algorithms.
use crate::{Document, Error, NodeId, NodeKind};
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Boundary {
    pub container: NodeId,
    pub offset: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum ContentsMode {
    Clone,
    Extract,
    Delete,
}

struct OutputNode {
    source: NodeId,
    deep: bool,
    slice: Option<(usize, usize)>,
}
enum ContentAction {
    Attach(usize, usize),
    Move(NodeId, usize),
    Remove(NodeId),
    Edit(NodeId, usize, usize),
}

fn inclusive_ancestor(
    document: &Document,
    ancestor: NodeId,
    mut node: NodeId,
) -> Result<bool, Error> {
    loop {
        if ancestor == node {
            return Ok(true);
        }
        let Some(parent) = document.parent(node)? else {
            return Ok(false);
        };
        node = parent;
    }
}

/// DOM compareDocumentPosition, including Attr's conceptual owner ordering.
/// Disconnected ordering uses existing stable native identity, without sidecars.
pub fn document_position(document: &Document, this: NodeId, other_document: &Document, other: NodeId) -> Result<u16, Error> {
    document.kind(this)?;
    other_document.kind(other)?;
    if this == other { return Ok(0); }
    let identity = |id: NodeId| (id.document, id.index, id.generation);
    let disconnected = || 1 | 32 | if identity(other) < identity(this) { 2 } else { 4 };
    let (node1, attr1) = match other_document.kind(other)? {
        NodeKind::Attribute { owner_element, .. } => (owner_element.as_deref().copied(), Some(other)),
        _ => (Some(other), None),
    };
    let (node2, attr2) = match document.kind(this)? {
        NodeKind::Attribute { owner_element, .. } => (owner_element.as_deref().copied(), Some(this)),
        _ => (Some(this), None),
    };
    if let (Some(a), Some(b), Some(owner)) = (attr1, attr2, node1) {
        if node2 == Some(owner) {
            if let Some(attributes) = document.materialized_attribute_nodes(owner) {
                let a_index = attributes.iter().find_map(|(index, id)| (*id == a).then_some(*index));
                let b_index = attributes.iter().find_map(|(index, id)| (*id == b).then_some(*index));
                if let (Some(a), Some(b)) = (a_index, b_index) { return Ok(32 | if a < b { 2 } else { 4 }); }
            }
        }
    }
    let (Some(node1), Some(node2)) = (node1, node2) else { return Ok(disconnected()); };
    if document.root_node(node2, false)? != other_document.root_node(node1, false)? { return Ok(disconnected()); }
    if (node1 != node2 && attr1.is_none() && inclusive_ancestor(document, node1, node2)?)
        || (node1 == node2 && attr2.is_some()) { return Ok(8 | 2); }
    if (node1 != node2 && attr2.is_none() && inclusive_ancestor(document, node2, node1)?)
        || (node1 == node2 && attr1.is_some()) { return Ok(16 | 4); }
    Ok(if compare(document, Boundary { container: node1, offset: 0 }, Boundary { container: node2, offset: 0 })? == Some(Ordering::Less) { 2 } else { 4 })
}

fn contained_child_bounds(
    document: &Document,
    parent: NodeId,
    start: Boundary,
    end: Boundary,
) -> Result<(usize, usize), Error> {
    let child_on_path = |mut node| -> Result<NodeId, Error> {
        while document.parent(node)? != Some(parent) {
            node = document.parent(node)?.ok_or(Error::WrongKind)?;
        }
        Ok(node)
    };
    let lower = if parent == start.container {
        start.offset
    } else if inclusive_ancestor(document, parent, start.container)? {
        child_index(document, child_on_path(start.container)?)? + 1
    } else {
        0
    };
    let upper = if parent == end.container {
        end.offset
    } else if inclusive_ancestor(document, parent, end.container)? {
        child_index(document, child_on_path(end.container)?)?
    } else {
        child_count(document, parent)?
    };
    Ok((lower, upper))
}

/// Plan before mutation, build detached clones once, then execute in tree order.
/// Fully contained extracted nodes move with their original native identities.
pub fn contents(
    document: &mut Document,
    start: Boundary,
    end: Boundary,
    mode: ContentsMode,
) -> Result<(Option<NodeId>, Boundary), Error> {
    let output = mode != ContentsMode::Delete;
    let mut collapse = start;
    if !inclusive_ancestor(document, start.container, end.container)? {
        let mut reference = start.container;
        while let Some(parent) = document.parent(reference)? {
            if inclusive_ancestor(document, parent, end.container)? {
                collapse = Boundary {
                    container: parent,
                    offset: child_index(document, reference)? + 1,
                };
                break;
            }
            reference = parent;
        }
    }
    let mut nodes = Vec::<OutputNode>::new();
    let mut actions = Vec::<ContentAction>::new();
    if start != end {
        if start.container == end.container
            && matches!(
                document.kind(start.container)?,
                NodeKind::Text(_)
                    | NodeKind::CData(_)
                    | NodeKind::Comment(_)
                    | NodeKind::ProcessingInstruction { .. }
            )
        {
            if output {
                nodes.push(OutputNode {
                    source: start.container,
                    deep: false,
                    slice: Some((start.offset, end.offset - start.offset)),
                });
                actions.push(ContentAction::Attach(1, 0));
            }
            if mode != ContentsMode::Clone {
                actions.push(ContentAction::Edit(
                    start.container,
                    start.offset,
                    end.offset - start.offset,
                ));
            }
        } else {
            let common = common_ancestor(document, start.container, end.container)?
                .ok_or(Error::WrongKind)?;
            let mut stack = Vec::new();
            let (lower, upper) = contained_child_bounds(document, common, start, end)?;
            stack.push((document.first_child(common)?, 0usize, 0usize, lower, upper));
            while let Some((next, parent_output, next_index, lower, upper)) = stack.last_mut() {
                let Some(node) = *next else {
                    stack.pop();
                    continue;
                };
                *next = document.next_sibling(node)?;
                let parent_output = *parent_output;
                let index = *next_index;
                *next_index += 1;
                let full = index >= *lower && index < *upper;
                if full {
                    if output && matches!(document.kind(node)?, NodeKind::DocumentType(_)) {
                        return Err(Error::Hierarchy);
                    }
                    match mode {
                        ContentsMode::Clone => {
                            nodes.push(OutputNode {
                                source: node,
                                deep: true,
                                slice: None,
                            });
                            actions.push(ContentAction::Attach(nodes.len(), parent_output));
                        }
                        ContentsMode::Extract => {
                            actions.push(ContentAction::Move(node, parent_output))
                        }
                        ContentsMode::Delete => actions.push(ContentAction::Remove(node)),
                    }
                    continue;
                }
                let partial = inclusive_ancestor(document, node, start.container)?
                    ^ inclusive_ancestor(document, node, end.container)?;
                if !partial {
                    continue;
                }
                if matches!(
                    document.kind(node)?,
                    NodeKind::Text(_)
                        | NodeKind::CData(_)
                        | NodeKind::Comment(_)
                        | NodeKind::ProcessingInstruction { .. }
                ) {
                    let lo = if node == start.container {
                        start.offset
                    } else {
                        0
                    };
                    let hi = if node == end.container {
                        end.offset
                    } else {
                        length(document, node)?
                    };
                    if output {
                        nodes.push(OutputNode {
                            source: node,
                            deep: false,
                            slice: Some((lo, hi - lo)),
                        });
                        actions.push(ContentAction::Attach(nodes.len(), parent_output));
                    }
                    if mode != ContentsMode::Clone {
                        actions.push(ContentAction::Edit(node, lo, hi - lo));
                    }
                } else {
                    let child_output = if output {
                        nodes.push(OutputNode {
                            source: node,
                            deep: false,
                            slice: None,
                        });
                        actions.push(ContentAction::Attach(nodes.len(), parent_output));
                        nodes.len()
                    } else {
                        parent_output
                    };
                    let (lower, upper) = contained_child_bounds(document, node, start, end)?;
                    stack.push((document.first_child(node)?, child_output, 0, lower, upper));
                }
            }
        }
    }
    let mut needed = usize::from(output);
    for node in &nodes {
        needed = needed
            .checked_add(document.clone_plan_count(node.source, node.deep)?.0)
            .ok_or(Error::LimitExceeded)?;
    }
    document.ensure_clone_capacity(needed)?;
    let fragment = if output {
        Some(document.create(NodeKind::DocumentFragment)?)
    } else {
        None
    };
    if let Some(fragment) = fragment {
        document.set_node_document(fragment, document.node_document(start.container)?)?;
    }
    let mut built = Vec::with_capacity(nodes.len() + 1);
    if let Some(fragment) = fragment {
        built.push(fragment);
    }
    for node in nodes {
        let clone = document.clone_node(node.source, node.deep)?;
        if let Some((offset, count)) = node.slice {
            let value = document.substring_data(node.source, offset, count)?;
            document.replace_data(clone, &value)?;
        }
        built.push(clone);
    }
    for action in actions {
        match action {
            ContentAction::Attach(node, parent) => document.append(built[parent], built[node])?,
            ContentAction::Move(node, parent) => document.append(built[parent], node)?,
            ContentAction::Remove(node) => document.remove(node)?,
            ContentAction::Edit(node, offset, count) => {
                document.replace_data_range(node, offset, count, "")?
            }
        }
    }
    Ok((fragment, collapse))
}

pub fn length(document: &Document, node: NodeId) -> Result<usize, Error> {
    match document.kind(node)? {
        NodeKind::Text(_)
        | NodeKind::CData(_)
        | NodeKind::Comment(_)
        | NodeKind::ProcessingInstruction { .. } => document.character_data_length(node),
        _ => child_count(document, node),
    }
}

pub fn child_count(document: &Document, node: NodeId) -> Result<usize, Error> {
    let mut count = 0;
    let mut child = document.first_child(node)?;
    while let Some(node) = child {
        count += 1;
        child = document.next_sibling(node)?;
    }
    Ok(count)
}

pub fn child_index(document: &Document, node: NodeId) -> Result<usize, Error> {
    let parent = document.parent(node)?.ok_or(Error::WrongKind)?;
    let mut child = document.first_child(parent)?;
    let mut index = 0;
    while let Some(id) = child {
        if id == node {
            return Ok(index);
        }
        index += 1;
        child = document.next_sibling(id)?;
    }
    Err(Error::WrongKind)
}

fn depth(document: &Document, mut node: NodeId) -> Result<usize, Error> {
    let mut depth = 0;
    while let Some(parent) = document.parent(node)? {
        depth += 1;
        node = parent;
    }
    Ok(depth)
}

pub fn common_ancestor(
    document: &Document,
    mut a: NodeId,
    mut b: NodeId,
) -> Result<Option<NodeId>, Error> {
    let mut da = depth(document, a)?;
    let mut db = depth(document, b)?;
    while da > db {
        a = document.parent(a)?.ok_or(Error::WrongKind)?;
        da -= 1;
    }
    while db > da {
        b = document.parent(b)?.ok_or(Error::WrongKind)?;
        db -= 1;
    }
    while a != b {
        let (Some(pa), Some(pb)) = (document.parent(a)?, document.parent(b)?) else {
            return Ok(None);
        };
        a = pa;
        b = pb;
    }
    Ok(Some(a))
}

/// DOM §5.2. Different roots have no boundary-point order.
pub fn compare(document: &Document, a: Boundary, b: Boundary) -> Result<Option<Ordering>, Error> {
    if a.container == b.container {
        document.kind(a.container)?;
        return Ok(Some(a.offset.cmp(&b.offset)));
    }
    let Some(common) = common_ancestor(document, a.container, b.container)? else {
        return Ok(None);
    };
    let below = |mut node| -> Result<NodeId, Error> {
        while document.parent(node)? != Some(common) {
            node = document.parent(node)?.ok_or(Error::WrongKind)?;
        }
        Ok(node)
    };
    if common == a.container {
        return Ok(Some(
            if a.offset <= child_index(document, below(b.container)?)? {
                Ordering::Less
            } else {
                Ordering::Greater
            },
        ));
    }
    if common == b.container {
        return Ok(Some(
            if child_index(document, below(a.container)?)? < b.offset {
                Ordering::Less
            } else {
                Ordering::Greater
            },
        ));
    }
    let ac = below(a.container)?;
    let bc = below(b.container)?;
    let mut sibling = document.first_child(common)?;
    while let Some(node) = sibling {
        if node == ac {
            return Ok(Some(Ordering::Less));
        }
        if node == bc {
            return Ok(Some(Ordering::Greater));
        }
        sibling = document.next_sibling(node)?;
    }
    Err(Error::WrongKind)
}

impl Document {
    /// DOM Text splitting, including exact live-range relocation phases.
    pub fn split_text(&mut self, node: NodeId, offset: usize) -> Result<NodeId, Error> {
        if !matches!(self.kind(node)?, NodeKind::Text(_) | NodeKind::CData(_)) {
            return Err(Error::WrongKind);
        }
        let length = self.character_data_length(node)?;
        if offset > length {
            return Err(Error::IndexSize);
        }
        let suffix = self.substring_data(node, offset, length - offset)?;
        let new_node = self.create(NodeKind::Text(suffix))?;
        let owner = self.node_document(node)?;
        self.set_node_document(new_node, owner)?;
        if let Some(parent) = self.parent(node)? {
            let index = child_index(self, node)?;
            if let Err(error) = self.insert_before(parent, new_node, self.next_sibling(node)?) {
                self.destroy_subtree(new_node)?;
                return Err(error);
            }
            self.notify_internal(&crate::observe::ObservedMutation {
                target: node,
                kind: crate::observe::ObservedKind::TextSplit {
                    new_node,
                    offset,
                    parent,
                    index,
                },
            });
        }
        self.replace_data_range(node, offset, length - offset, "")?;
        Ok(new_node)
    }

    /// DOM Node.normalize: only exclusive Text nodes merge; CDATA is a boundary.
    pub fn normalize(&mut self, root: NodeId) -> Result<(), Error> {
        self.kind(root)?;
        let mut current = self.first_child(root)?;
        while let Some(node) = current {
            if matches!(self.kind(node)?, NodeKind::Text(_)) {
                let mut length = self.character_data_length(node)?;
                if length == 0 {
                    current = self.next_in_subtree(root, node)?;
                    self.remove(node)?;
                    continue;
                }
                let mut data = String::new();
                let mut next = self.next_sibling(node)?;
                while let Some(sibling) = next {
                    let NodeKind::Text(text) = self.kind(sibling)? else {
                        break;
                    };
                    data.push_str(text);
                    next = self.next_sibling(sibling)?;
                }
                self.replace_data_range(node, length, 0, &data)?;
                while let Some(source) = self.next_sibling(node)? {
                    if !matches!(self.kind(source)?, NodeKind::Text(_)) {
                        break;
                    }
                    let source_length = self.character_data_length(source)?;
                    let parent = self.parent(source)?.ok_or(Error::WrongKind)?;
                    let index = child_index(self, source)?;
                    self.notify_internal(&crate::observe::ObservedMutation {
                        target: source,
                        kind: crate::observe::ObservedKind::TextMerge {
                            destination: node,
                            offset: length,
                            parent,
                            index,
                        },
                    });
                    length += source_length;
                    self.remove(source)?;
                }
            }
            current = self.next_in_subtree(root, node)?;
        }
        Ok(())
    }

    /// Preorder successor without a traversal allocation or crossing `root`.
    pub fn next_in_subtree(&self, root: NodeId, mut node: NodeId) -> Result<Option<NodeId>, Error> {
        if let Some(child) = self.first_child(node)? {
            return Ok(Some(child));
        }
        while node != root {
            if let Some(next) = self.next_sibling(node)? {
                return Ok(Some(next));
            }
            let Some(parent) = self.parent(node)? else {
                return Ok(None);
            };
            node = parent;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::rc::Rc;
    use core::cell::RefCell;

    fn element(name: &str) -> NodeKind {
        NodeKind::Element {
            namespace: crate::Namespace::Html,
            name: name.into(),
            attributes: Vec::new(),
        }
    }

    #[test]
    fn specification_range_core_orders_roots_and_keeps_mutation_phases_observer_invisible() {
        let mut document = crate::html::parse("<main>aaaaaa</main>", 64).unwrap();
        let main = crate::selector::query_selector(&document, document.root(), "main")
            .unwrap()
            .unwrap();
        let text = document.first_child(main).unwrap().unwrap();
        let detached = document.create(element("div")).unwrap();
        assert_eq!(
            compare(
                &document,
                Boundary {
                    container: text,
                    offset: 0
                },
                Boundary {
                    container: detached,
                    offset: 0
                }
            )
            .unwrap(),
            None
        );
        assert_eq!(
            compare(
                &document,
                Boundary {
                    container: main,
                    offset: 0
                },
                Boundary {
                    container: text,
                    offset: 0
                }
            )
            .unwrap(),
            Some(Ordering::Less)
        );
        let internal = Rc::new(RefCell::new(Vec::new()));
        let observed = Rc::new(RefCell::new(Vec::new()));
        let log = internal.clone();
        document.set_mutation_sink(Some(Rc::new(move |_, event| {
            log.borrow_mut().push(event.clone())
        })));
        let log = observed.clone();
        document.set_mutation_observer_sink(Some(Rc::new(move |_, event| {
            log.borrow_mut().push(event.clone())
        })));
        let version = document.version;
        document.replace_data_range(text, 1, 2, "aa").unwrap();
        assert_eq!(
            document.version, version,
            "equal authored mutation preserves layout cache"
        );
        assert!(matches!(
            observed.borrow()[0].kind,
            crate::observe::ObservedKind::CharacterData {
                offset: 1,
                removed: 2,
                inserted: 2,
                ..
            }
        ));
        internal.borrow_mut().clear();
        observed.borrow_mut().clear();
        document.split_text(text, 3).unwrap();
        assert_eq!(internal.borrow().len(), 3);
        assert_eq!(observed.borrow().len(), 2);
        assert!(matches!(
            internal.borrow()[1].kind,
            crate::observe::ObservedKind::TextSplit {
                offset: 3,
                index: 0,
                ..
            }
        ));
        internal.borrow_mut().clear();
        observed.borrow_mut().clear();
        document.normalize(main).unwrap();
        assert_eq!(document.character_data_length(text).unwrap(), 6);
        assert_eq!(internal.borrow().len(), 3);
        assert_eq!(observed.borrow().len(), 2);
        assert!(matches!(
            internal.borrow()[1].kind,
            crate::observe::ObservedKind::TextMerge {
                offset: 3,
                index: 1,
                ..
            }
        ));
    }

    #[test]
    fn specification_range_core_extraction_moves_identities_and_preflights_capacity() {
        let mut document =
            crate::html::parse("<main><b>abc</b><i>DEF</i><u>ghi</u></main>", 64).unwrap();
        let main = crate::selector::query_selector(&document, document.root(), "main")
            .unwrap()
            .unwrap();
        let first = document.first_child(main).unwrap().unwrap();
        let middle = document.next_sibling(first).unwrap().unwrap();
        let last = document.next_sibling(middle).unwrap().unwrap();
        let start = Boundary {
            container: document.first_child(first).unwrap().unwrap(),
            offset: 1,
        };
        let end = Boundary {
            container: document.first_child(last).unwrap().unwrap(),
            offset: 2,
        };
        let (fragment, collapse) =
            contents(&mut document, start, end, ContentsMode::Extract).unwrap();
        let fragment = fragment.unwrap();
        assert_eq!(document.parent(middle).unwrap(), Some(fragment));
        assert_eq!(
            collapse,
            Boundary {
                container: main,
                offset: 1
            }
        );
        assert_eq!(document.character_data_length(start.container).unwrap(), 1);
        assert_eq!(document.character_data_length(end.container).unwrap(), 1);
        let mut limited = crate::html::parse("<main>abcd</main>", 16).unwrap();
        let main = crate::selector::query_selector(&limited, limited.root(), "main")
            .unwrap()
            .unwrap();
        let text = limited.first_child(main).unwrap().unwrap();
        while limited.remaining_node_capacity() > 0 {
            limited.create(element("unused")).unwrap();
        }
        let point = Boundary {
            container: text,
            offset: 1,
        };
        assert_eq!(
            contents(
                &mut limited,
                point,
                Boundary { offset: 3, ..point },
                ContentsMode::Extract
            ),
            Err(Error::LimitExceeded)
        );
        assert_eq!(limited.substring_data(text, 0, 4).unwrap(), "abcd");
    }
}
