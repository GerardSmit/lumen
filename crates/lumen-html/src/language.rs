//! HTML language determination and document default-language processing.
//! All consumers use DOM parentage; slot assignment is not language inheritance.
use crate::{Document, Error, Namespace, NodeId, NodeKind};
use alloc::string::String;

/// RFC 4647 extended filtering over borrowed subtags. A singleton can match
/// its counterpart but cannot be skipped while searching for a later subtag.
/// CSS's empty range matches only an untagged language; '*' excludes it.
pub(crate) fn matches_range(language:&str,range:&str,mut work:impl FnMut()->bool)->bool {
    if range.is_empty(){return language.is_empty();}
    if language.is_empty(){return false;}
    let valid=|value:&str,wildcards:bool,work:&mut dyn FnMut()->bool| {
        value.split('-').enumerate().all(|(index,part)| {
            work() && ((wildcards&&part=="*") || (!part.is_empty()&&part.len()<=8&&part.bytes().all(|byte|if index==0{byte.is_ascii_alphabetic()}else{byte.is_ascii_alphanumeric()})))
        })
    };
    if !valid(language,false,&mut work)||!valid(range,true,&mut work){return false;}
    let mut tags=language.split('-');let mut ranges=range.split('-');
    let first=ranges.next().expect("nonempty validated range");
    let tag=tags.next().expect("nonempty validated language");
    if first!="*"&&!first.eq_ignore_ascii_case(tag){return false;}
    for wanted in ranges {
        if !work(){return false;}
        if wanted=="*"{continue;}
        loop {
            let Some(tag)=tags.next()else{return false;};
            if !work(){return false;}
            if wanted.eq_ignore_ascii_case(tag){break;}
            if tag.len()==1{return false;}
        }
    }
    true
}

#[derive(Default)]
pub(crate) struct DefaultLanguages {
    pub pragma: Option<String>,
    pub protocol: Option<String>,
}

pub(crate) fn ascii_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{000c}' | '\r' | ' ')
}

/// HTML's obsolete pragma accepts only the first ASCII-whitespace token, and
/// rejects the complete value if a comma occurs anywhere.
pub(crate) fn pragma_candidate(input: &str) -> Option<&str> {
    if input.contains(',') {
        return None;
    }
    input
        .split(ascii_whitespace)
        .find(|token| !token.is_empty())
}

/// HTTP reports one language only when a single nonempty field token exists.
/// Multiple header fields or comma-separated tags make the fallback unknown.
pub(crate) fn protocol_candidate(headers: &[(String, String)]) -> Option<&str> {
    let mut candidate = None;
    for (_, value) in headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-language"))
    {
        if candidate.is_some() || value.contains(',') {
            return None;
        }
        let mut tokens = value
            .split(ascii_whitespace)
            .filter(|token| !token.is_empty());
        let token = tokens.next()?;
        if tokens.next().is_some() {
            return None;
        }
        candidate = Some(token);
    }
    candidate
}

pub(crate) fn declared(document: &Document, node: NodeId) -> Result<Option<&str>, Error> {
    let NodeKind::Element { namespace, .. } = document.kind(node)? else {
        return Ok(None);
    };
    if let Some(value) =
        document.get_attribute_ns_ref(node, Some("http://www.w3.org/XML/1998/namespace"), "lang")?
    {
        return Ok(Some(value));
    }
    if matches!(namespace, Namespace::Html | Namespace::Svg) {
        document.get_attribute_ns_ref(node, None, "lang")
    } else {
        Ok(None)
    }
}

/// Unknown or syntactically unusual tags remain distinct, unchanged tags.
/// `work` lets selectors charge the same canonical parent walk to their quota.
pub(crate) fn determine(
    document: &Document,
    node: NodeId,
    work: impl FnMut() -> bool,
) -> Result<Option<&str>, Error> {
    determine_with_parent(document,node,work,|node|document.parent(node))
}

pub(crate) fn determine_with_parent(
    document: &Document,
    mut node: NodeId,
    mut work: impl FnMut() -> bool,
    mut parent: impl FnMut(NodeId)->Result<Option<NodeId>,Error>,
) -> Result<Option<&str>, Error> {
    for _ in 0..512 {
        if !work() {
            return Err(Error::LimitExceeded);
        }
        if let Some(value) = declared(document, node)? {
            return Ok(Some(value));
        }
        if let Some(parent) = parent(node)? {
            if let Some(host) = document.shadow_host(parent)? {
                node = host;
                continue;
            }
            if matches!(document.kind(parent)?, NodeKind::Element { .. }) {
                node = parent;
                continue;
            }
        }
        // One arena can also house the inert template owner document. Its
        // language defaults are independent of the ordinary document's metadata.
        if document.node_document(node)? != document.root() {
            return Ok(None);
        }
        return Ok(document
            .language_defaults
            .as_ref()
            .and_then(|defaults| defaults.pragma.as_deref().or(defaults.protocol.as_deref())));
    }
    Err(Error::LimitExceeded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{shadow::ShadowMode, Name};
    fn by_id(document: &Document, id: &str) -> NodeId {
        crate::selector::query_selector(document, document.root(), &alloc::format!("#{id}"))
            .unwrap()
            .unwrap()
    }
    #[test]
    fn specification_language_uses_actual_namespaces_dom_parent_and_shadow_host_with_inert_defaults(
    ) {
        let mut document = crate::html::parse("<!doctype html><div id=host lang=de><span id=light slot=x></span></div><math lang=fr><mtext id=mathChild></mtext></math><svg lang=nl><text id=svgChild></text></svg><p id=unknown lang=XYZzy></p><p id=empty lang=''></p><template id=t><span id=inert></span></template>", 128).unwrap();
        document
            .set_content_language_headers(&[("Content-Language".into(), "it".into())])
            .unwrap();
        let host = by_id(&document, "host");
        let light = by_id(&document, "light");
        let root = document.attach_shadow(host, ShadowMode::Open).unwrap();
        let slot = document
            .create_unprefixed_element(
                Namespace::Html,
                Name::new("slot"),
                alloc::vec![
                    (Name::new("name"), "x".into()),
                    (Name::new("lang"), "fr".into())
                ],
            )
            .unwrap();
        document.append(root, slot).unwrap();
        let shadow_child = document
            .create_unprefixed_element(Namespace::Html, Name::new("span"), alloc::vec![])
            .unwrap();
        document.append(root, shadow_child).unwrap();
        assert_eq!(
            document.content_language(light).unwrap(),
            Some("de"),
            "slot assignment does not change language parentage"
        );
        assert_eq!(document.content_language(shadow_child).unwrap(), Some("de"));
        assert_eq!(document.content_language(slot).unwrap(), Some("fr"));
        document
            .set_attribute_ns(
                host,
                Some("http://www.w3.org/XML/1998/namespace"),
                "xml:lang",
                "nl-NL",
            )
            .unwrap();
        assert_eq!(
            document.content_language(light).unwrap(),
            Some("nl-NL"),
            "real XML namespace has precedence"
        );
        document
            .set_attribute_ns(host, None, "xml:lang", "fr")
            .unwrap();
        document
            .remove_attribute_ns(host, Some("http://www.w3.org/XML/1998/namespace"), "lang")
            .unwrap();
        assert_eq!(
            document.content_language(light).unwrap(),
            Some("de"),
            "literal no-namespace xml:lang is ignored"
        );
        assert_eq!(
            document
                .content_language(by_id(&document, "mathChild"))
                .unwrap(),
            Some("it"),
            "MathML does not acquire HTML's no-namespace lang rule"
        );
        assert_eq!(
            document
                .content_language(by_id(&document, "svgChild"))
                .unwrap(),
            Some("nl")
        );
        assert_eq!(
            document
                .content_language(by_id(&document, "unknown"))
                .unwrap(),
            Some("XYZzy")
        );
        assert_eq!(
            document
                .content_language(by_id(&document, "empty"))
                .unwrap(),
            Some("")
        );
        let template = by_id(&document, "t");
        let content = document.template_content(template).unwrap().unwrap();
        let inert = document.first_child(content).unwrap().unwrap();
        assert_eq!(
            document.content_language(inert).unwrap(),
            None,
            "the template owner's defaults are independent"
        );
        let mut work = 0;
        assert_eq!(
            determine(&document, light, || {
                work += 1;
                work == 1
            }),
            Err(Error::LimitExceeded)
        );
    }
    #[test]
    fn specification_language_pragma_is_insertion_only_and_protocol_multiple_languages_are_unknown()
    {
        let mut document = crate::html::parse("<!doctype html><head><meta id=first http-equiv=content-language content='nl extra'><meta http-equiv=content-language content='fr,en'></head><body><p id=target></p></body>", 128).unwrap();
        document
            .set_content_language_headers(&[("Content-Language".into(), "de".into())])
            .unwrap();
        let target = by_id(&document, "target");
        let first = by_id(&document, "first");
        assert_eq!(document.content_language(target).unwrap(), Some("nl"));
        document
            .set_attribute_ns(first, None, "content", "fr")
            .unwrap();
        document.remove(first).unwrap();
        assert_eq!(document.content_language(target).unwrap(), Some("nl"));
        let head = crate::selector::query_selector(&document, document.root(), "head")
            .unwrap()
            .unwrap();
        document.append(head, first).unwrap();
        assert_eq!(document.content_language(target).unwrap(), Some("fr"));
        document
            .set_attribute_ns(first, None, "content", "it")
            .unwrap();
        document.move_before(head, first, None).unwrap();
        assert_eq!(
            document.content_language(target).unwrap(),
            Some("fr"),
            "state-preserving move has no insertion steps"
        );
        let mut fresh = Document::new(16);
        fresh
            .set_content_language_headers(&[("Content-Language".into(), "de".into())])
            .unwrap();
        assert_eq!(fresh.content_language(fresh.root()).unwrap(), Some("de"));
        fresh
            .set_content_language_headers(&[("Content-Language".into(), "de,fr".into())])
            .unwrap();
        assert_eq!(fresh.content_language(fresh.root()).unwrap(), None);
        fresh
            .set_content_language_headers(&[
                ("Content-Language".into(), "de".into()),
                ("content-language".into(), "de".into()),
            ])
            .unwrap();
        assert_eq!(fresh.content_language(fresh.root()).unwrap(), None);
        assert_eq!(
            pragma_candidate("\u{00a0}xyz rest"),
            Some("\u{00a0}xyz"),
            "only ASCII whitespace is skipped"
        );
        assert_eq!(pragma_candidate("nl extra,ignored"), None);
    }
}
