//! Borrowed HTML global-attribute states and DOM-tree editing eligibility.
use crate::{Document, Namespace, NodeId, NodeKind};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentEditableState { Inherit, True, False, PlaintextOnly }
impl ContentEditableState {
    pub fn keyword(self) -> &'static str { match self {
        Self::Inherit => "inherit", Self::True => "true", Self::False => "false", Self::PlaintextOnly => "plaintext-only",
    }}
}

fn html(document: &Document, node: NodeId) -> bool {
    matches!(document.kind(node), Ok(NodeKind::Element { namespace: Namespace::Html, .. }))
}
fn attribute<'a>(document: &'a Document, node: NodeId, name: &str) -> Option<&'a str> {
    document.get_attribute_ns_ref(node, None, name).ok().flatten()
}
pub fn parent_element(document: &Document, node: NodeId) -> Option<NodeId> {
    document.parent(node).ok().flatten().filter(|parent|matches!(document.kind(*parent),Ok(NodeKind::Element { .. })))
}

pub fn content_editable_state(document: &Document, node: NodeId) -> ContentEditableState {
    if !html(document,node) { return ContentEditableState::Inherit; }
    match attribute(document,node,"contenteditable") {
        Some(raw) if raw.is_empty() || raw.eq_ignore_ascii_case("true") => ContentEditableState::True,
        Some(raw) if raw.eq_ignore_ascii_case("false") => ContentEditableState::False,
        Some(raw) if raw.eq_ignore_ascii_case("plaintext-only") => ContentEditableState::PlaintextOnly,
        _ => ContentEditableState::Inherit,
    }
}
/// IDL setter input differs from content-attribute parsing: the empty string is invalid.
pub fn content_editable_setter_state(raw: &str) -> Option<ContentEditableState> {
    if raw.eq_ignore_ascii_case("inherit") { Some(ContentEditableState::Inherit) }
    else if raw.eq_ignore_ascii_case("true") { Some(ContentEditableState::True) }
    else if raw.eq_ignore_ascii_case("false") { Some(ContentEditableState::False) }
    else if raw.eq_ignore_ascii_case("plaintext-only") { Some(ContentEditableState::PlaintextOnly) }
    else { None }
}
pub fn is_editing_host(document: &Document, node: NodeId) -> bool {
    if !html(document,node) { return false; }
    if matches!(content_editable_state(document,node),ContentEditableState::True|ContentEditableState::PlaintextOnly) { return true; }
    document.parent(node).ok().flatten().is_some_and(|parent|
        matches!(document.kind(parent),Ok(NodeKind::Document)) && document.design_mode_enabled(parent))
}
/// HTML delegates editable to the editing specification: DOM parents, not
/// assigned slots or shadow hosts, and only eligible namespace boundaries.
pub fn is_content_editable(document: &Document, mut node: NodeId) -> bool {
    loop {
        if is_editing_host(document,node) { return true; }
        if content_editable_state(document,node)==ContentEditableState::False { return false; }
        let Ok(kind)=document.kind(node) else { return false; };
        let Some(parent)=document.parent(node).ok().flatten() else { return false; };
        match kind {
            NodeKind::Element { namespace:Namespace::Html, .. } => {},
            NodeKind::Element { namespace:Namespace::Svg, .. }
                if document.element_name_parts(node).is_ok_and(|(_,local)|local=="svg") => {},
            NodeKind::Element { namespace:Namespace::MathMl, .. }
                if document.element_name_parts(node).is_ok_and(|(_,local)|local=="math") => {},
            NodeKind::Element { .. } | NodeKind::Document | NodeKind::DocumentFragment => return false,
            _ if !html(document,parent) => return false,
            _ => {},
        }
        node=parent;
    }
}
pub fn is_editable(document: &Document, node: NodeId) -> bool {
    !is_editing_host(document,node) && is_content_editable(document,node)
}

pub fn translation_enabled(document: &Document, mut node: NodeId) -> bool {
    loop {
        if html(document,node) { if let Some(raw)=attribute(document,node,"translate") {
            if raw.is_empty() || raw.eq_ignore_ascii_case("yes") { return true; }
            if raw.eq_ignore_ascii_case("no") { return false; }
        }}
        let Some(parent)=parent_element(document,node) else { return true; };
        node=parent;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpellcheckDefault { True, False, Inherit }
/// Established UA policy: descendants inherit their DOM parent element's
/// behavior, and roots enable checking. This IDL policy is independent of
/// whether the embedding offers a spelling UI or a user override.
pub fn spellcheck_default(document: &Document,node:NodeId) -> SpellcheckDefault {
    if parent_element(document,node).is_some() {SpellcheckDefault::Inherit}else{SpellcheckDefault::True}
}
pub fn spellcheck_enabled(document:&Document,mut node:NodeId)->bool {
    loop {
        if let Some(raw)=attribute(document,node,"spellcheck") {
            if raw.is_empty() || raw.eq_ignore_ascii_case("true") {return true;}
            if raw.eq_ignore_ascii_case("false") {return false;}
        }
        match spellcheck_default(document,node) {
            SpellcheckDefault::True=>return true,SpellcheckDefault::False=>return false,
            SpellcheckDefault::Inherit=>match parent_element(document,node) {Some(parent)=>node=parent,None=>return false},
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DraggableState { Auto, True, False }
pub fn draggable_state(document:&Document,node:NodeId)->DraggableState {
    match attribute(document,node,"draggable") {
        Some(raw) if raw.eq_ignore_ascii_case("true")=>DraggableState::True,
        Some(raw) if raw.eq_ignore_ascii_case("false")=>DraggableState::False,
        _=>DraggableState::Auto,
    }
}
/// Object image representation is supplied by the actual embedding loader,
/// rather than inferred from a URL extension or a declared MIME type.
pub fn draggable_enabled(document:&Document,node:NodeId,object_represents_image:bool)->bool {
    match draggable_state(document,node) {
        DraggableState::True=>true,DraggableState::False=>false,
        DraggableState::Auto=>match crate::forms::html_element_local_name(document,node) {
            Some("img")=>true,Some("a")=>attribute(document,node,"href").is_some(),
            Some("object")=>object_represents_image,_=>false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn find(document:&Document,id:&str)->NodeId {
        crate::selector::get_element_by_id(document,document.root(),id).unwrap().unwrap()
    }
    #[test]
    fn specification_element_metadata_states_dom_inheritance_and_namespace_boundaries() {
        let mut document=crate::html::parse("<div id=p translate=no spellcheck=false contenteditable><span id=c></span><span id=f contenteditable=false><i id=i></i><b id=h contenteditable=plaintext-only></b></span><a id=a href></a></div>",64).unwrap();
        let p=find(&document,"p");let c=find(&document,"c");let f=find(&document,"f");let i=find(&document,"i");let h=find(&document,"h");let a=find(&document,"a");
        assert!(!translation_enabled(&document,c));assert!(!spellcheck_enabled(&document,c));
        assert!(is_editing_host(&document,p));assert!(is_editable(&document,c));
        assert!(!is_content_editable(&document,f));assert!(!is_content_editable(&document,i));assert!(is_editing_host(&document,h));
        document.set_attribute(c,"translate","YeS").unwrap();assert!(translation_enabled(&document,c));
        document.set_attribute(c,"translate","yeſ").unwrap();assert!(!translation_enabled(&document,c));
        document.set_attribute(c,"spellcheck","").unwrap();assert!(spellcheck_enabled(&document,c));
        document.set_attribute(c,"spellcheck","falſe").unwrap();assert!(!spellcheck_enabled(&document,c));
        let svg=document.create(NodeKind::Element{namespace:Namespace::Svg,name:"svg".into(),attributes:alloc::vec![("translate".into(),"yes".into())]}).unwrap();
        document.append(p,svg).unwrap();assert!(!translation_enabled(&document,svg));assert!(is_editable(&document,svg));
        let g=document.create(NodeKind::Element{namespace:Namespace::Svg,name:"g".into(),attributes:alloc::vec![]}).unwrap();document.append(svg,g).unwrap();assert!(!is_editable(&document,g));
        document.remove(c).unwrap();assert!(translation_enabled(&document,c));assert!(spellcheck_enabled(&document,c));assert!(!is_content_editable(&document,c));
        assert!(draggable_enabled(&document,a,false));document.remove_attribute(a,"href").unwrap();assert!(!draggable_enabled(&document,a,false));
        document.set_attribute(a,"draggable","").unwrap();assert_eq!(draggable_state(&document,a),DraggableState::Auto);
        let object=document.create(NodeKind::Element{namespace:Namespace::Html,name:"object".into(),attributes:alloc::vec![]}).unwrap();
        assert!(!draggable_enabled(&document,object,false));assert!(draggable_enabled(&document,object,true),"actual image representation hook drives Auto");
        assert_eq!(content_editable_setter_state(""),None);assert_eq!(content_editable_setter_state("TRUE"),Some(ContentEditableState::True));
    }
    #[test]
    fn specification_editability_design_mode_is_document_owned_and_changes_selector_state() {
        let mut document=crate::html::parse("<html><body><div id=t></div><div id=f contenteditable=false></div></body></html>",32).unwrap();
        let root=document.root();let t=find(&document,"t");let f=find(&document,"f");let version=document.version();
        assert!(!is_content_editable(&document,t));assert!(document.set_design_mode_enabled(root,true).unwrap());assert!(document.version()>version);
        assert!(is_content_editable(&document,t));assert!(!is_content_editable(&document,f));
        assert!(crate::selector::matches(&document,t,":read-write").unwrap());
        assert!(!document.set_design_mode_enabled(root,true).unwrap());
        let inert=document.ensure_template_owner_document().unwrap();assert!(!document.design_mode_enabled(inert));
        assert!(document.set_design_mode_enabled(root,false).unwrap());assert!(!is_content_editable(&document,t));
    }
}
