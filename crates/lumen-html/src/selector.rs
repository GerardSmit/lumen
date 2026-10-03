//! DOM queries backed by the cascade's selector matcher.
use crate::{css, Document, Error, NodeId};
use alloc::vec::Vec;

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

pub fn matches(document: &Document, node: NodeId, selector: &str) -> Result<bool, SelectorError> {
    let selector = css::parse_selector_list(selector, 0)?;
    document.kind(node)?;
    Ok(selector
        .iter()
        .any(|selector| selector.matches_node(document, node)))
}

pub fn closest(
    document: &Document,
    node: NodeId,
    selector: &str,
) -> Result<Option<NodeId>, SelectorError> {
    let selector = css::parse_selector_list(selector, 0)?;
    let mut current = Some(node);
    while let Some(id) = current {
        if selector
            .iter()
            .any(|selector| selector.matches_node(document, id))
        {
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
    document.kind(root)?;
    let selector = css::parse_selector_list(selector, 0)?;
    let mut current = document.first_child(root)?;
    while let Some(id) = current {
        if selector
            .iter()
            .any(|selector| selector.matches_node(document, id))
        {
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
    document.kind(root)?;
    let selector = css::parse_selector_list(selector, 0)?;
    let mut result = Vec::new();
    let mut current = document.first_child(root)?;
    while let Some(id) = current {
        if selector
            .iter()
            .any(|selector| selector.matches_node(document, id))
        {
            result.try_reserve(1).map_err(|_| Error::LimitExceeded)?;
            result.push(id);
        }
        current = next_descendant(document, root, id)?;
    }
    Ok(result)
}

pub(crate) fn next_descendant(document: &Document, root: NodeId, id: NodeId) -> Result<Option<NodeId>, Error> {
    if let Some(child) = document.first_child(id)? {
        return Ok(Some(child));
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            query_selector(&doc, root, "p[data-label='']").unwrap(),
            Some(two)
        );
        assert!(query_selector(&doc, root, "[data-label").is_err());
        assert!(query_selector(&doc, root, "[data-label~='a']").is_err());
    }
}
