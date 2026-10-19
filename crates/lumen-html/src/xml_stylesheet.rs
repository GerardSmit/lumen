use crate::{Document, Error, NodeId, NodeKind};
use alloc::{borrow::Cow, vec::Vec};
use lumen_common::xml::{parse_pseudo_attributes, PseudoAttribute, PseudoAttributeError};

const MAX_PSEUDO_ATTRIBUTE_BYTES: usize = 1024 * 1024;
const MAX_PSEUDO_ATTRIBUTES: usize = 256;

/// XML stylesheet data is independent of Element attributes and preserves
/// source whitespace. Unrecognized pseudo-attributes still participate in
/// duplicate detection in the canonical XML pseudo-attribute parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Descriptor<'a> { attributes: Vec<PseudoAttribute<'a>> }
impl<'a> Descriptor<'a> {
    pub fn get(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|attribute| attribute.name == name).map(|attribute| attribute.value.as_ref())
    }
    pub fn href(&self) -> Option<&str> { self.get("href") }
    pub fn title(&self) -> &str { self.get("title").unwrap_or("") }
    pub fn media(&self) -> &str { self.get("media").unwrap_or("") }
    pub fn alternate(&self) -> bool { self.get("alternate") == Some("yes") }
    pub fn css_supported(&self) -> bool {
        self.get("type").is_none_or(lumen_common::mime::stylesheet_hint_supported)
            && (!self.alternate() || !self.title().is_empty())
    }
    pub fn into_href(self) -> Option<Cow<'a, str>> {
        self.attributes.into_iter().find(|attribute|attribute.name=="href").map(|attribute|attribute.value)
    }
}

pub fn is_candidate(document: &Document, node: NodeId) -> bool {
    matches!(document.kind(node), Ok(NodeKind::ProcessingInstruction { target, .. }) if target == "xml-stylesheet")
}

/// CSSOM's prolog is the actual direct Document child sequence preceding its
/// element child. Detached, nested and epilog instructions have no sheet.
pub fn in_prolog(document: &Document, node: NodeId) -> Result<bool, Error> {
    let Some(parent) = document.parent(node)? else { return Ok(false); };
    if !matches!(document.kind(parent)?, NodeKind::Document) { return Ok(false); }
    let mut previous = document.previous_sibling(node)?;
    while let Some(sibling) = previous {
        if matches!(document.kind(sibling)?, NodeKind::Element { .. }) { return Ok(false); }
        previous = document.previous_sibling(sibling)?;
    }
    Ok(true)
}

pub fn descriptor(document: &Document, node: NodeId) -> Result<Option<Descriptor<'_>>, Error> {
    if !is_candidate(document,node) || !in_prolog(document,node)? {return Ok(None);}
    parse_data(document,node)
}

// Callers walking the actual Document child order already know prolog
// membership. Keep XML parsing shared without rescanning preceding siblings.
pub(crate) fn parse_data(document: &Document,node:NodeId)->Result<Option<Descriptor<'_>>,Error> {
    let NodeKind::ProcessingInstruction {target,data}=document.kind(node)? else{return Ok(None);};
    if target!="xml-stylesheet" {return Ok(None);}
    match parse_pseudo_attributes(data, MAX_PSEUDO_ATTRIBUTE_BYTES, MAX_PSEUDO_ATTRIBUTES) {
        Ok(attributes) => Ok(Some(Descriptor { attributes })),
        Err(PseudoAttributeError::Invalid) => Ok(None),
        Err(PseudoAttributeError::LimitExceeded) => Err(Error::LimitExceeded),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_xml_stylesheet_doctype_instructions_do_not_become_document_children() {
        let document=crate::xml::parse("<!DOCTYPE root [<?xml-stylesheet href='bad.css'?> <!-- subset -->]><root/>",128).unwrap();
        let doctype=document.first_child(document.root()).unwrap().unwrap();
        assert!(matches!(document.kind(doctype),Ok(NodeKind::DocumentType(_))));
        let root=document.next_sibling(doctype).unwrap().unwrap();
        assert!(matches!(document.kind(root),Ok(NodeKind::Element{..})));
        assert!(document.next_sibling(root).unwrap().is_none());
    }
    #[test]
    fn specification_xml_stylesheet_prolog_uses_actual_document_order_and_pseudo_attributes() {
        let mut document=crate::xml::parse("<?xml-stylesheet href='a&amp;b.css' media='screen' title='A' alternate='yes'?> <root/> <?xml-stylesheet href='after.css'?>",128).unwrap();
        let first=document.first_child(document.root()).unwrap().unwrap();
        let last=document.last_child(document.root()).unwrap().unwrap();
        let descriptor=descriptor(&document,first).unwrap().unwrap();
        assert_eq!(descriptor.href(),Some("a&b.css"));assert_eq!(descriptor.media(),"screen");assert!(descriptor.alternate());assert!(descriptor.css_supported());
        assert!(super::descriptor(&document,last).unwrap().is_none());
        let mut root=first;
        while !matches!(document.kind(root),Ok(NodeKind::Element{..})) {root=document.next_sibling(root).unwrap().unwrap();}
        document.append(root,first).unwrap();
        assert!(super::descriptor(&document,first).unwrap().is_none());
        document.insert_before(document.root(),first,Some(root)).unwrap();
        assert!(super::descriptor(&document,first).unwrap().is_some());
    }
}
