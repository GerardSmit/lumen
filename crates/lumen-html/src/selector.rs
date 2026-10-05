//! DOM queries backed by the cascade's selector matcher.
use crate::{css, Document, Error, NodeId, NodeKind};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;

const SELECTOR_CACHE_ENTRIES: usize = 32;

fn cached_selector_list(
    document: &Document,
    source: &str,
) -> Result<Rc<Vec<css::Selector>>, css::CssError> {
    let mut cache = document.selector_cache.borrow_mut();
    if let Some(position) = cache.iter().position(|(text, _)| text == source) {
        let entry = cache.remove(position);
        let parsed = entry.1.clone();
        cache.push(entry);
        return Ok(parsed);
    }
    let parsed = Rc::new(css::parse_selector_list(source, 0)?);
    if cache.len() >= SELECTOR_CACHE_ENTRIES {
        cache.remove(0);
    }
    cache.push((String::from(source), parsed.clone()));
    Ok(parsed)
}

#[derive(Debug, PartialEq)]
pub enum SelectorError {
    Dom(Error),
    Css(css::CssError),
}

impl From<Error> for SelectorError {
    fn from(error: Error) -> Self {
        Self::Dom(error)
    }
}

impl From<css::CssError> for SelectorError {
    fn from(error: css::CssError) -> Self {
        Self::Css(error)
    }
}

/// Return the first element directly under the document root.
pub fn document_element(document: &Document) -> Option<NodeId> {
    let mut child = document.first_child(document.root()).ok().flatten();
    while let Some(id) = child {
        if matches!(document.kind(id), Ok(NodeKind::Element { .. })) {
            return Some(id);
        }
        child = document.next_sibling(id).ok().flatten();
    }
    None
}

/// A plain `#ident` selector queried from the document root, where the id index applies.
/// Quirks mode matches ids case-insensitively, so it keeps the walk.
fn pure_id_selector<'a>(document: &Document, root: NodeId, selector: &'a str) -> Option<&'a str> {
    if root != document.root() || document.document_mode() == crate::DocumentMode::Quirks {
        return None;
    }
    let name = selector.trim().strip_prefix('#')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_ascii_alphabetic() || first == '_' || !first.is_ascii())
        || !chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii())
    {
        return None;
    }
    Some(name)
}

fn query_scope_root(document: &Document, root: NodeId) -> Option<NodeId> {
    if matches!(document.kind(root), Ok(NodeKind::Document)) {
        document_element(document)
    } else {
        Some(root)
    }
}

pub fn matches(document: &Document, node: NodeId, selector: &str) -> Result<bool, SelectorError> {
    matches_with_validity(document, node, selector, &crate::forms::NoValidityOverrides)
}

pub fn matches_with_validity(
    document: &Document,
    node: NodeId,
    selector: &str,
    validity: &dyn crate::forms::ValidityStateView,
) -> Result<bool, SelectorError> {
    let selector = cached_selector_list(document, selector)?;
    let scope_root = matches!(document.kind(node)?, NodeKind::Element { .. }).then_some(node);
    Ok(selector.iter().any(|selector| {
        selector.matches_node_in_scope_with_validity(document, node, scope_root, validity)
    }))
}

pub fn closest(
    document: &Document,
    node: NodeId,
    selector: &str,
) -> Result<Option<NodeId>, SelectorError> {
    closest_with_validity(document, node, selector, &crate::forms::NoValidityOverrides)
}

pub fn closest_with_validity(
    document: &Document,
    node: NodeId,
    selector: &str,
    validity: &dyn crate::forms::ValidityStateView,
) -> Result<Option<NodeId>, SelectorError> {
    let selector = cached_selector_list(document, selector)?;
    let scope_root = matches!(document.kind(node)?, NodeKind::Element { .. }).then_some(node);
    let mut current = Some(node);
    while let Some(id) = current {
        if selector.iter().any(|selector| {
            selector.matches_node_in_scope_with_validity(document, id, scope_root, validity)
        }) {
            return Ok(Some(id));
        }
        current = document.parent(id)?;
    }
    Ok(None)
}

pub fn query_selector(
    document: &Document,
    root: NodeId,
    selector: &str,
) -> Result<Option<NodeId>, SelectorError> {
    query_selector_with_validity(document, root, selector, &crate::forms::NoValidityOverrides)
}

pub fn query_selector_with_validity(
    document: &Document,
    root: NodeId,
    selector: &str,
    validity: &dyn crate::forms::ValidityStateView,
) -> Result<Option<NodeId>, SelectorError> {
    document.kind(root)?;
    let wanted_id = pure_id_selector(document, root, selector);
    let selector = cached_selector_list(document, selector)?;
    let scope_root = query_scope_root(document, root);
    if let Some(wanted) = wanted_id {
        if let Some(found) = document.indexed_element_by_id(wanted) {
            return Ok(found);
        }
    }
    let mut current = document.first_child(root)?;
    while let Some(id) = current {
        if selector.iter().any(|selector| {
            selector.matches_node_in_scope_with_validity(document, id, scope_root, validity)
        }) {
            return Ok(Some(id));
        }
        current = next_descendant(document, root, id)?;
    }
    Ok(None)
}

pub fn query_selector_all(
    document: &Document,
    root: NodeId,
    selector: &str,
) -> Result<Vec<NodeId>, SelectorError> {
    query_selector_all_with_validity(document, root, selector, &crate::forms::NoValidityOverrides)
}

pub fn query_selector_all_with_validity(
    document: &Document,
    root: NodeId,
    selector: &str,
    validity: &dyn crate::forms::ValidityStateView,
) -> Result<Vec<NodeId>, SelectorError> {
    document.kind(root)?;
    let selector = cached_selector_list(document, selector)?;
    let scope_root = query_scope_root(document, root);
    let mut result = Vec::new();
    let mut current = document.first_child(root)?;
    while let Some(id) = current {
        if selector.iter().any(|selector| {
            selector.matches_node_in_scope_with_validity(document, id, scope_root, validity)
        }) {
            result.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            result.push(id);
        }
        current = next_descendant(document, root, id)?;
    }
    Ok(result)
}

/// Find the first ordinary descendant with a nonempty, null-namespace ID.
/// Document fragments and shadow roots use the same bounded tree-order walk;
/// neither shadow trees nor template contents are crossed implicitly.
pub fn get_element_by_id(
    document: &Document,
    root: NodeId,
    wanted: &str,
) -> Result<Option<NodeId>, Error> {
    document.kind(root)?;
    if wanted.is_empty() {
        return Ok(None);
    }
    if root == document.root() {
        if let Some(found) = document.indexed_element_by_id(wanted) {
            return Ok(found);
        }
    }
    let mut current = document.first_child(root)?;
    while let Some(id) = current {
        if matches!(document.kind(id)?, NodeKind::Element { .. })
            && document.get_attribute_ns_ref(id, None, "id")? == Some(wanted)
        {
            return Ok(Some(id));
        }
        current = next_descendant(document, root, id)?;
    }
    Ok(None)
}

/// Advance in tree order within `root` without allocating a traversal stack.
pub fn next_descendant(
    document: &Document,
    root: NodeId,
    id: NodeId,
) -> Result<Option<NodeId>, Error> {
    next_tree_node(document, root, id, false)
}

/// Advance to the next node after `id`'s subtree, bounded by `root`, without
/// allocating a traversal stack.
pub fn next_after_subtree(
    document: &Document,
    root: NodeId,
    id: NodeId,
) -> Result<Option<NodeId>, Error> {
    let mut cursor = id;
    loop {
        if cursor == root {
            return Ok(None);
        }
        if let Some(next) = document.next_sibling(cursor)? {
            return Ok(Some(next));
        }
        cursor = document.parent(cursor)?.ok_or(Error::InvalidNode)?;
    }
}

/// Advance in shadow-including tree order without allocating a traversal
/// stack. Native shadow roots (including closed roots) precede light children;
/// detached template contents are not descendants of their template element.
pub fn next_shadow_including_descendant(
    document: &Document,
    root: NodeId,
    id: NodeId,
) -> Result<Option<NodeId>, Error> {
    next_tree_node(document, root, id, true)
}

/// Return the parent used when tracing cached native DOM identities.
///
/// The walk follows ordinary parents, shadow hosts, template-content owners,
/// and the owner element of an attached `Attr`. It does not follow slot
/// assignment: assigned nodes already belong to the host's light tree.
pub fn native_identity_parent(document: &Document, id: NodeId) -> Result<Option<NodeId>, Error> {
    if let NodeKind::Attribute { owner_element, .. } = document.kind(id)? {
        if let Some(owner) = owner_element.as_deref() {
            return Ok(Some(*owner));
        }
    }
    document.host_including_parent(id)
}

/// Find the root of the native identity component containing `id`.
///
/// The component is a tree over DOM parent/child links plus shadow hosts,
/// template-content ownership, and attached attributes. The node-count bound
/// turns corrupted cyclic links into a reported DOM error rather than an
/// unbounded walk during garbage collection.
pub fn native_identity_root(document: &Document, id: NodeId) -> Result<NodeId, Error> {
    document.kind(id)?;
    let mut cursor = id;
    for _ in 0..=document.node_count() {
        let Some(parent) = native_identity_parent(document, cursor)? else {
            return Ok(cursor);
        };
        if parent == cursor {
            return Err(Error::LimitExceeded);
        }
        cursor = parent;
    }
    Err(Error::LimitExceeded)
}

/// Advance through the native identity component in depth-first order without
/// allocating a traversal stack. Closed shadow roots and detached template
/// contents are included. Materialized `Attr` identities are emitted by the
/// owner-element visitor and are intentionally not part of this node walk.
pub fn next_native_identity_descendant(
    document: &Document,
    root: NodeId,
    id: NodeId,
) -> Result<Option<NodeId>, Error> {
    if let Some(child) = native_identity_first_child(document, id)? {
        return Ok(Some(child));
    }
    let mut cursor = id;
    for _ in 0..=document.node_count() {
        if cursor == root {
            return Ok(None);
        }
        let Some(parent) = native_identity_parent(document, cursor)? else {
            return Err(Error::InvalidNode);
        };
        if let Some(next) = native_identity_next_sibling(document, parent, cursor)? {
            return Ok(Some(next));
        }
        cursor = parent;
    }
    Err(Error::LimitExceeded)
}

fn native_identity_first_child(document: &Document, id: NodeId) -> Result<Option<NodeId>, Error> {
    if let Some(shadow) = document.shadow_root(id)? {
        return Ok(Some(shadow));
    }
    if let Some(content) = document.template_content(id)? {
        return Ok(Some(content));
    }
    document.first_child(id)
}

fn native_identity_next_sibling(
    document: &Document,
    parent: NodeId,
    child: NodeId,
) -> Result<Option<NodeId>, Error> {
    if document.shadow_host(child)? == Some(parent) {
        if let Some(content) = document.template_content(parent)? {
            return Ok(Some(content));
        }
        return document.first_child(parent);
    }
    if document.template_content(parent)? == Some(child) {
        return document.first_child(parent);
    }
    document.next_sibling(child)
}

fn next_tree_node(
    document: &Document,
    root: NodeId,
    id: NodeId,
    shadow_including: bool,
) -> Result<Option<NodeId>, Error> {
    if shadow_including {
        if let Some(shadow) = document.shadow_root(id)? {
            return Ok(Some(shadow));
        }
    }
    if let Some(child) = document.first_child(id)? {
        return Ok(Some(child));
    }
    if !shadow_including {
        return next_after_subtree(document, root, id);
    }
    let mut cursor = id;
    loop {
        if cursor == root {
            return Ok(None);
        }
        if let Some(next) = document.next_sibling(cursor)? {
            return Ok(Some(next));
        }
        if let Some(parent) = document.parent(cursor)? {
            cursor = parent;
        } else if shadow_including {
            let host = document.shadow_host(cursor)?.ok_or(Error::InvalidNode)?;
            // A shadow root is visited before its host's ordinary children.
            if let Some(light) = document.first_child(host)? {
                return Ok(Some(light));
            }
            cursor = host;
        } else {
            return Err(Error::InvalidNode);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec::Vec};

    #[derive(Default)]
    struct LiveValidity {
        values: Vec<(NodeId, String)>,
        custom: Vec<(NodeId, String)>,
        checked: Vec<(NodeId, bool)>,
        selected: Vec<(NodeId, bool)>,
    }

    impl LiveValidity {
        fn set<T>(values: &mut Vec<(NodeId, T)>, node: NodeId, value: T) {
            if let Some((_, current)) = values.iter_mut().find(|(id, _)| *id == node) {
                *current = value;
            } else {
                values.push((node, value));
            }
        }

        fn set_value(&mut self, node: NodeId, value: &str) {
            Self::set(&mut self.values, node, String::from(value));
        }

        fn set_custom(&mut self, node: NodeId, value: &str) {
            Self::set(&mut self.custom, node, String::from(value));
        }

        fn set_checked(&mut self, node: NodeId, value: bool) {
            Self::set(&mut self.checked, node, value);
        }

        fn set_selected(&mut self, node: NodeId, value: bool) {
            Self::set(&mut self.selected, node, value);
        }
    }

    impl crate::forms::ValidityStateView for LiveValidity {
        fn value_override(&self, node: NodeId) -> Option<&str> {
            self.values
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| value.as_str())
        }

        fn custom_message(&self, node: NodeId) -> Option<&str> {
            self.custom
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| value.as_str())
        }

        fn checkedness(&self, node: NodeId) -> Option<bool> {
            self.checked
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| *value)
        }

        fn selectedness(&self, node: NodeId) -> Option<bool> {
            self.selected
                .iter()
                .find(|(id, _)| *id == node)
                .map(|(_, value)| *value)
        }
    }

    #[test]
    fn id_lookup_is_nonempty_tree_ordered_and_scoped_to_fragments_and_shadow_roots() {
        let mut document = crate::html::parse_with_declarative_shadow_roots(
            "<div id='outside'><template shadowrootmode=open><b id='same'></b></template><span id='same'></span><i id=''></i></div>",
            64,
            true,
        ).unwrap();
        let root = document.root();
        let host = get_element_by_id(&document, root, "outside")
            .unwrap()
            .unwrap();
        let shadow = document.shadow_root(host).unwrap().unwrap();
        let light = get_element_by_id(&document, root, "same").unwrap().unwrap();
        let shadow_element = get_element_by_id(&document, shadow, "same")
            .unwrap()
            .unwrap();
        assert_ne!(light, shadow_element);
        assert_eq!(get_element_by_id(&document, root, "").unwrap(), None);
        assert_eq!(
            get_element_by_id(&document, shadow, "outside").unwrap(),
            None
        );
        let fragment = document.create(NodeKind::DocumentFragment).unwrap();
        document.append(fragment, light).unwrap();
        let later = document
            .create(NodeKind::Element {
                namespace: crate::Namespace::Html,
                name: "em".into(),
                attributes: alloc::vec![("id".into(), "same".into())],
            })
            .unwrap();
        document.append(fragment, later).unwrap();
        assert_eq!(
            get_element_by_id(&document, fragment, "same").unwrap(),
            Some(light)
        );
        assert_eq!(get_element_by_id(&document, root, "same").unwrap(), None);
        document.set_attribute(light, "id", "renamed").unwrap();
        assert_eq!(
            get_element_by_id(&document, fragment, "same").unwrap(),
            Some(later)
        );
        assert_eq!(
            get_element_by_id(&document, fragment, "renamed").unwrap(),
            Some(light)
        );
    }

    #[test]
    fn shadow_including_walk_is_ordered_bounded_and_leaves_templates_inert() {
        let document = crate::html::parse_with_declarative_shadow_roots(
            "<div id=a><template shadowrootmode=closed><b id=shadow></b><section id=nested><template shadowrootmode=open><em id=inner></em></template><i id=innerLight></i></section></template><span id=light></span><template><img id=inert></template></div><p id=after></p>",
            64,
            true,
        ).unwrap();
        let collect_ids = |root| {
            let mut ids = Vec::new();
            let mut current = Some(root);
            while let Some(node) = current {
                if let NodeKind::Element { attributes, .. } = document.kind(node).unwrap() {
                    if let Some((_, id)) = attributes.iter().find(|(name, _)| name == "id") {
                        ids.push(id.as_str());
                    }
                }
                current = next_shadow_including_descendant(&document, root, node).unwrap();
            }
            ids
        };
        assert_eq!(
            collect_ids(document.root()),
            [
                "a",
                "shadow",
                "nested",
                "inner",
                "innerLight",
                "light",
                "after"
            ]
        );
        let host = query_selector(&document, document.root(), "#a")
            .unwrap()
            .unwrap();
        let shadow = document.shadow_root(host).unwrap().unwrap();
        assert_eq!(
            collect_ids(shadow),
            ["shadow", "nested", "inner", "innerLight"]
        );
        assert_eq!(
            collect_ids(host),
            ["a", "shadow", "nested", "inner", "innerLight", "light"]
        );
        assert!(query_selector(&document, document.root(), "#shadow")
            .unwrap()
            .is_none());
        assert!(query_selector(&document, document.root(), "#inert")
            .unwrap()
            .is_none());
        assert_eq!(
            query_selector_all(&document, host, "[id]").unwrap().len(),
            1
        );
    }

    #[test]
    fn validity_pseudos_use_live_control_state_and_aggregate_forms() {
        let document = crate::html::parse(
            "<form id=form><fieldset id=group><input id=required required><input id=check type=checkbox required><input id=pattern pattern='[0-9]+' value=letters><select id=choice required><option id=placeholder value=''>Choose</option><option id=chosen value=ready>Ready</option></select></fieldset></form>",
            64,
        )
        .unwrap();
        let id = |selector| {
            query_selector(&document, document.root(), selector)
                .unwrap()
                .unwrap()
        };
        let form = id("#form");
        let group = id("#group");
        let required = id("#required");
        let check = id("#check");
        let pattern = id("#pattern");
        let placeholder = id("#placeholder");
        let chosen = id("#chosen");
        let choice = id("#choice");

        assert!(
            matches_with_validity(&document, required, ":invalid", &LiveValidity::default())
                .unwrap()
        );
        assert!(
            matches_with_validity(&document, pattern, ":invalid", &LiveValidity::default())
                .unwrap()
        );
        assert!(
            matches_with_validity(&document, group, ":invalid", &LiveValidity::default()).unwrap()
        );
        assert!(
            matches_with_validity(&document, form, ":invalid", &LiveValidity::default()).unwrap()
        );

        let mut live = LiveValidity::default();
        live.set_value(required, "filled");
        live.set_value(pattern, "123");
        live.set_checked(check, true);
        live.set_selected(placeholder, false);
        live.set_selected(chosen, true);
        assert!(matches_with_validity(&document, required, ":is(:valid)", &live).unwrap());
        assert!(matches_with_validity(&document, pattern, ":valid", &live).unwrap());
        assert!(matches_with_validity(&document, check, ":valid", &live).unwrap());
        assert!(matches_with_validity(&document, choice, ":valid", &live).unwrap());
        assert!(matches_with_validity(&document, group, ":valid", &live).unwrap());
        assert!(matches_with_validity(&document, form, ":valid", &live).unwrap());

        live.set_custom(required, "host error");
        assert!(matches_with_validity(&document, required, ":invalid", &live).unwrap());
        assert!(matches_with_validity(&document, group, ":invalid", &live).unwrap());
        assert!(matches_with_validity(&document, form, ":invalid", &live).unwrap());
        assert!(!matches_with_validity(&document, form, ":not(:invalid)", &live).unwrap());
    }

    #[test]
    fn queries_use_document_order_and_shared_matching() {
        let doc = crate::html::parse(
            "<div class='outer'><p id='first'>A</p><p class='note'>B</p></div>",
            16,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        let div = doc.first_child(body).unwrap().unwrap();
        let first = doc.first_child(div).unwrap().unwrap();
        assert_eq!(query_selector(&doc, div, "p").unwrap(), Some(first));
        assert_eq!(query_selector(&doc, div, "#first").unwrap(), Some(first));
        assert!(matches(&doc, first, "p#first").unwrap());
        assert_eq!(closest(&doc, first, ".outer").unwrap(), Some(div));
        assert_eq!(query_selector(&doc, first, ".outer").unwrap(), None);
        let second = doc.next_sibling(first).unwrap().unwrap();
        assert_eq!(query_selector_all(&doc, div, "p").unwrap(), [first, second]);
        assert!(query_selector_all(&doc, first, ".outer")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn lists_keep_document_order_deduplicate_and_decode_identifiers() {
        let doc =
            crate::html::parse("<p id='123' class='a+b'>A</p><p title='a,b'>B</p>", 16).unwrap();
        let root = doc.root();
        let all = query_selector_all(&doc, root, "p").unwrap();
        assert_eq!(
            query_selector_all(&doc, root, "[title='a,b'], p, p").unwrap(),
            all
        );
        assert_eq!(
            query_selector(&doc, root, r"#\31 23").unwrap(),
            Some(all[0])
        );
        assert!(matches(&doc, all[0], r"span, .a\+b").unwrap());
        assert_eq!(closest(&doc, all[0], "div, p").unwrap(), Some(all[0]));
        for invalid in ["p, :unknown", "p,", ",p"] {
            assert!(query_selector_all(&doc, root, invalid).is_err());
            assert!(matches(&doc, all[0], invalid).is_err());
        }
    }

    #[test]
    fn combinators_match_element_ancestry_and_skip_text_siblings() {
        let doc = crate::html::parse("<div class='outer'><section><p id='one'>A</p>text<!--skip--><p id='two'>B</p><span><p id='three'>C</p></span></section></div>", 24).unwrap();
        let root = doc.root();
        let one = query_selector(&doc, root, "#one").unwrap().unwrap();
        let two = query_selector(&doc, root, "#two").unwrap().unwrap();
        let three = query_selector(&doc, root, "#three").unwrap().unwrap();
        assert_eq!(
            query_selector_all(&doc, root, ".outer p").unwrap(),
            [one, two, three]
        );
        assert_eq!(
            query_selector_all(&doc, root, "section > p").unwrap(),
            [one, two]
        );
        assert_eq!(query_selector(&doc, root, "#one + p").unwrap(), Some(two));
        assert_eq!(
            query_selector(&doc, root, "#one ~ span > p").unwrap(),
            Some(three)
        );
        assert!(query_selector(&doc, root, "p >").is_err());
        assert!(query_selector(&doc, root, "p >> span").is_err());
        assert_eq!(
            closest(&doc, three, ".outer > section").unwrap(),
            doc.parent(doc.parent(three).unwrap().unwrap()).unwrap()
        );
    }

    #[test]
    fn attribute_presence_and_equality_keep_quoted_whitespace() {
        let doc = crate::html::parse(
            "<div data-label='a > b'><p id='one' disabled></p><p id='two' data-label=''></p></div>",
            16,
        )
        .unwrap();
        let root = doc.root();
        let one = query_selector(&doc, root, "#one").unwrap().unwrap();
        let two = query_selector(&doc, root, "#two").unwrap().unwrap();
        assert_eq!(
            query_selector(&doc, root, "[data-label='a > b'] > [disabled]").unwrap(),
            Some(one)
        );
        let container = query_selector(&doc, root, "div").unwrap().unwrap();
        assert_eq!(
            query_selector(&doc, root, "p[data-label='']").unwrap(),
            Some(two)
        );
        assert_eq!(
            query_selector(&doc, root, "[data-label~='a']").unwrap(),
            Some(container)
        );
        assert!(query_selector(&doc, root, "[data-label").is_err());
    }

    #[test]
    fn attribute_operators_flags_escapes_and_empty_substrings() {
        let doc = crate::html::parse(
            r#"<div id='tokens' data-token='one two' data-language='en-US'></div><div id='text' data-value='Prefix-MiXeD-Suffix' data-label='a]b, c' data-quote='a"b'></div><div id='empty' data-empty=''></div>"#,
            16,
        )
        .unwrap();
        let root = doc.root();
        let tokens = query_selector(&doc, root, "#tokens").unwrap().unwrap();
        let text = query_selector(&doc, root, "#text").unwrap().unwrap();

        for (selector, expected) in [
            ("[data-token~=two]", Some(tokens)),
            ("[data-language|=en]", Some(tokens)),
            ("[data-value^=Prefix]", Some(text)),
            ("[data-value$=Suffix]", Some(text)),
            ("[data-value*=MiXeD]", Some(text)),
            ("[data-value*=mixed i]", Some(text)),
            ("[data-value*=mixed I]", Some(text)),
            ("[data-value*=mixed s]", None),
            (r"[data-value^=Pre\66 ix]", Some(text)),
            (r#"[data\-label='a]b, c']"#, Some(text)),
            (r#"[data-quote='a\22 b']"#, Some(text)),
        ] {
            assert_eq!(
                query_selector(&doc, root, selector).unwrap(),
                expected,
                "{selector}"
            );
        }
        for selector in [
            "[data-empty^='']",
            "[data-empty$='']",
            "[data-empty*='']",
            "[data-empty~='']",
        ] {
            assert_eq!(
                query_selector(&doc, root, selector).unwrap(),
                None,
                "{selector}"
            );
        }
        assert_eq!(
            query_selector(&doc, root, "[data-language|='en-US']").unwrap(),
            Some(tokens)
        );
        assert_eq!(
            query_selector(&doc, root, "[data-language|='en-u']").unwrap(),
            None
        );
    }

    #[test]
    fn attribute_names_fold_only_for_html_document_elements() {
        let html =
            crate::html::parse("<div id='target' data-label='html-value'></div>", 8).unwrap();
        let html_target = query_selector(&html, html.root(), "[DATA-LABEL='html-value']")
            .unwrap()
            .unwrap();
        assert!(matches!(
            html.kind(html_target),
            Ok(NodeKind::Element { .. })
        ));
        let foreign = crate::html::parse("<svg id='foreign' viewBox='0 0 1 1'></svg>", 8).unwrap();
        let foreign_root = foreign.root();
        assert!(query_selector(&foreign, foreign_root, "svg[viewBox]")
            .unwrap()
            .is_some());
        assert_eq!(
            query_selector(&foreign, foreign_root, "svg[viewbox]").unwrap(),
            None
        );

        let xml = crate::xml::parse(
            "<root><Item id='target' DATA-LABEL='upper-value' data-label='lower-value'/></root>",
            8,
        )
        .unwrap();
        let root = xml.root();
        let target = query_selector(&xml, root, "Item#target").unwrap().unwrap();
        assert_eq!(
            query_selector(&xml, root, "Item[DATA-LABEL='upper-value']").unwrap(),
            Some(target)
        );
        assert_eq!(
            query_selector(&xml, root, "Item[data-label='lower-value']").unwrap(),
            Some(target)
        );
        assert_eq!(
            query_selector(&xml, root, "Item[Data-label='lower-value']").unwrap(),
            None
        );
    }

    #[test]
    fn malformed_attribute_selector_operators_flags_and_escapes_are_strict() {
        let doc = crate::html::parse("<div id='target'></div>", 8).unwrap();
        for selector in [
            "[data^=]",
            "[data i]",
            "[data=value q]",
            "[data=value i s]",
            "[data^=value trailing]",
            "[data=value\\\n]",
            "[data==value]",
        ] {
            assert!(
                query_selector(&doc, doc.root(), selector).is_err(),
                "{selector:?}"
            );
        }
    }

    #[test]
    fn html_default_attribute_values_are_ascii_insensitive_only_in_html() {
        let html = crate::html::parse(
            "<input id='type' type='TEXT'><a id='rel' rel='StyleSheet'></a><p id='lang' lang='EN-us'></p><div id='case' class='Foo' data-label='MiXeD' data-utf='Äbc'></div><svg id='foreign' type='TEXT'></svg>",
            24,
        )
        .unwrap();
        let root = html.root();
        let type_element = query_selector(&html, root, "#type").unwrap().unwrap();
        let rel_element = query_selector(&html, root, "#rel").unwrap().unwrap();
        let lang_element = query_selector(&html, root, "#lang").unwrap().unwrap();
        let foreign = query_selector(&html, root, "#foreign").unwrap().unwrap();

        for (selector, expected) in [
            ("#type[type=text]", Some(type_element)),
            ("#type[type=text s]", None),
            ("#type[type=TEXT s]", Some(type_element)),
            ("#rel[rel~=stylesheet]", Some(rel_element)),
            ("#lang[lang|=en]", Some(lang_element)),
            ("#case[class=foo]", None),
            ("#case[id=CASE]", None),
            ("#case[data-label=mixed]", None),
            (
                "#case[data-label=mixed i]",
                Some(query_selector(&html, root, "#case").unwrap().unwrap()),
            ),
            ("#case[data-utf*=äb i]", None),
            (
                "#case[data-utf*=Äb i]",
                Some(query_selector(&html, root, "#case").unwrap().unwrap()),
            ),
            ("#foreign[type=text]", None),
            ("#foreign[type=text i]", Some(foreign)),
        ] {
            assert_eq!(
                query_selector(&html, root, selector).unwrap(),
                expected,
                "{selector}"
            );
        }

        let xml = crate::xml::parse(
            "<html xmlns='http://www.w3.org/1999/xhtml'><input id='xml' type='TEXT'/></html>",
            8,
        )
        .unwrap();
        assert_eq!(
            query_selector(&xml, xml.root(), "input[type=text]").unwrap(),
            None
        );
        assert!(query_selector(&xml, xml.root(), "input[type=TEXT]")
            .unwrap()
            .is_some());
    }

    #[test]
    fn scope_uses_parent_query_roots_and_the_original_closest_element() {
        assert!(css::parse_selector(":scope()", 0).is_err());
        assert!(css::parse_selector("::scope", 0).is_err());
        let doc = crate::html::parse(
            "<main id='scope'><section id='child'><i id='deep'></i></section></main>",
            16,
        )
        .unwrap();
        let root = doc.root();
        let html = document_element(&doc).unwrap();
        let main = query_selector(&doc, root, "#scope").unwrap().unwrap();
        let child = query_selector(&doc, root, "#child").unwrap().unwrap();
        let deep = query_selector(&doc, root, "#deep").unwrap().unwrap();

        assert_eq!(query_selector(&doc, root, ":scope"), Ok(Some(html)));
        assert_eq!(
            query_selector(&doc, root, ":scope > body"),
            Ok(Some(query_selector(&doc, root, "body").unwrap().unwrap()))
        );
        assert_eq!(query_selector(&doc, main, ":scope"), Ok(None));
        assert_eq!(
            query_selector(&doc, main, ":scope > section"),
            Ok(Some(child))
        );
        assert!(matches(&doc, main, ":scope").unwrap());
        assert!(matches(&doc, child, ":scope").unwrap());
        assert_eq!(closest(&doc, deep, ":scope"), Ok(Some(deep)));
        assert_eq!(closest(&doc, deep, "main:scope"), Ok(None));
    }

    #[test]
    fn scope_is_threaded_into_has_and_fragment_roots_are_virtual() {
        let doc = crate::html::parse(
            "<main><div class='a'><div id='scope' class='b'><div id='child' class='c'><div class='d'></div></div><div class='e'></div></div></div></main>",
            24,
        )
        .unwrap();
        let scope = query_selector(&doc, doc.root(), "#scope").unwrap().unwrap();
        let child = query_selector(&doc, doc.root(), "#child").unwrap().unwrap();
        assert!(query_selector_all(&doc, scope, ":has(:scope)")
            .unwrap()
            .is_empty());
        assert_eq!(
            query_selector_all(&doc, scope, ".a:has(:scope) .c").unwrap(),
            [child]
        );
        assert_eq!(
            query_selector_all(&doc, scope, ".c:has(:is(:scope .d))").unwrap(),
            [child]
        );

        let mut detached = Document::new(8);
        let fragment = detached.create(NodeKind::DocumentFragment).unwrap();
        let child = detached
            .create(NodeKind::Element {
                namespace: crate::Namespace::Html,
                name: crate::Name::new("div"),
                attributes: alloc::vec![(crate::Name::new("class"), "child".into())],
            })
            .unwrap();
        detached.append(fragment, child).unwrap();
        assert_eq!(query_selector(&detached, fragment, ":scope"), Ok(None));
        assert_eq!(
            query_selector(&detached, fragment, ":scope > .child"),
            Ok(Some(child))
        );
        assert_eq!(
            query_selector_all(&detached, fragment, ":scope .child").unwrap(),
            [child]
        );
    }
}
