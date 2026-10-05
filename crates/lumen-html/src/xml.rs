//! Bounded, non-validating XML parsing into the shared document arena.
//!
//! Tokenizing, well-formedness, character and entity references and DTD handling come from
//! `lumen_common::xml`; this module resolves namespaces and builds the tree. External subsets and
//! external entities are never loaded.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    rc::Rc,
    string::{String, ToString},
    vec::Vec,
};
use lumen_common::xml::{self as shared, Attribute, Flow, Handler, Options, Parser, Shared};

pub use lumen_common::xml::is_name as is_xml_name;

const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 512;
const XML_NAMESPACE: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE: &str = "http://www.w3.org/2000/xmlns/";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: &'static str,
}

fn error(offset: usize, message: &'static str) -> ParseError {
    ParseError { offset, message }
}

/// Parse a well-formed XML document. The returned tree preserves XML name
/// case, comments, processing instructions, and resolved element namespaces.
pub fn parse(input: &str, max_nodes: usize) -> Result<Document, ParseError> {
    if input.len() > MAX_XML_BYTES {
        return Err(error(0, "XML input too large"));
    }
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let mut parser = Parser::new(Options {
        encoding: Some(String::from("UTF-8")),
        ..Options::default()
    });
    let mut builder = Builder::new(parser.shared(), max_nodes);
    match parser.parse(&mut builder, input.as_bytes(), true) {
        Ok(()) => Ok(builder.document),
        Err(failure) => Err(builder.failure.unwrap_or_else(|| {
            error(
                builder.shared.error_position().byte as usize,
                failure.message(),
            )
        })),
    }
}

struct Builder {
    shared: Rc<Shared>,
    document: Document,
    open: Vec<(NodeId, usize)>,
    bindings: Vec<(String, String)>,
    text: String,
    doctype_name: Option<String>,
    failure: Option<ParseError>,
}

impl Builder {
    fn new(shared: Rc<Shared>, max_nodes: usize) -> Builder {
        Builder {
            shared,
            document: Document::new(max_nodes),
            open: Vec::new(),
            bindings: alloc::vec![
                (String::from("xml"), String::from(XML_NAMESPACE)),
                (String::from("xmlns"), String::from(XMLNS_NAMESPACE)),
            ],
            text: String::new(),
            doctype_name: None,
            failure: None,
        }
    }

    fn fail(&mut self, message: &'static str) -> Flow {
        let offset = self.shared.position().byte as usize;
        self.failure.get_or_insert(error(offset, message));
        Flow::Abort
    }

    fn parent(&self) -> NodeId {
        self.open
            .last()
            .map_or(self.document.root(), |(node, _)| *node)
    }

    fn add(&mut self, kind: NodeKind, placement: &'static str) -> Result<NodeId, &'static str> {
        let id = self.document.create(kind).map_err(|e| match e {
            DomError::LimitExceeded => "XML node limit exceeded",
            _ => "invalid XML node",
        })?;
        self.document
            .append(self.parent(), id)
            .map_err(|_| placement)?;
        Ok(id)
    }

    fn flush_text(&mut self) -> Result<(), &'static str> {
        if self.text.is_empty() {
            return Ok(());
        }
        let text = core::mem::take(&mut self.text);
        self.add(NodeKind::Text(text), "invalid text placement")
            .map(|_| ())
    }

    fn add_node(
        &mut self,
        kind: NodeKind,
        placement: &'static str,
    ) -> Result<NodeId, &'static str> {
        self.flush_text()?;
        self.add(kind, placement)
    }

    fn flow(&mut self, result: Result<(), &'static str>) -> Flow {
        match result {
            Ok(()) => Flow::Continue,
            Err(message) => self.fail(message),
        }
    }

    fn start(&mut self, qname: &str, attrs: &[Attribute]) -> Result<(), &'static str> {
        self.flush_text()?;
        if self.open.len() >= MAX_DEPTH {
            return Err("XML nesting limit exceeded");
        }
        if self.open.is_empty()
            && self
                .doctype_name
                .as_deref()
                .is_some_and(|name| name != qname)
        {
            return Err("doctype name does not match document element");
        }
        let mark = self.bindings.len();
        for attr in attrs {
            if attr.name == "xmlns" {
                bind_namespace(&mut self.bindings, "", &attr.value)?;
            } else if let Some(prefix) = attr.name.strip_prefix("xmlns:") {
                if prefix.is_empty() || split_qname(prefix).is_none_or(|(p, _)| !p.is_empty()) {
                    return Err("invalid namespace prefix");
                }
                bind_namespace(&mut self.bindings, prefix, &attr.value)?;
            }
        }
        let (prefix, _) = split_qname(qname).ok_or("invalid qualified name")?;
        let uri = if prefix.is_empty() {
            namespace_for(&self.bindings, "")
        } else {
            Some(namespace_for(&self.bindings, prefix).ok_or("unbound element prefix")?)
        };
        let mut expanded_names: Vec<(Option<String>, &str)> = Vec::new();
        let mut namespace_metadata: Vec<(&str, Option<String>)> = Vec::new();
        for attr in attrs {
            let name = attr.name.as_str();
            if name == "xmlns" || name.starts_with("xmlns:") {
                namespace_metadata.push((name, Some(String::from(XMLNS_NAMESPACE))));
                continue;
            }
            let (prefix, local) = split_qname(name).ok_or("invalid attribute qualified name")?;
            let uri = if prefix.is_empty() {
                None
            } else {
                Some(namespace_for(&self.bindings, prefix).ok_or("unbound attribute prefix")?)
            };
            if expanded_names
                .iter()
                .any(|(seen_uri, seen_local)| *seen_uri == uri && *seen_local == local)
            {
                return Err("duplicate expanded attribute name");
            }
            namespace_metadata.push((name, uri.clone()));
            expanded_names.push((uri, local));
        }
        let kind = NodeKind::Element {
            namespace: namespace_from_uri(uri.as_deref()),
            name: Name::new(qname),
            attributes: attrs
                .iter()
                .map(|attr| (Name::new(&attr.name), attr.value.clone()))
                .collect(),
        };
        let id = self.add(kind, "invalid element placement")?;
        for (qualified_name, uri) in namespace_metadata {
            self.document
                .set_attribute_namespace_metadata(id, qualified_name, uri.as_deref())
                .map_err(|_| "invalid attribute namespace metadata")?;
        }
        self.open.push((id, mark));
        Ok(())
    }
}

impl Handler for Builder {
    fn start_doctype(
        &mut self,
        name: &str,
        sysid: Option<&str>,
        pubid: Option<&str>,
        _has_internal_subset: bool,
    ) -> Flow {
        let result = self
            .add_node(
                NodeKind::DocumentType(name.to_string()),
                "invalid doctype placement",
            )
            .and_then(|id| {
                self.document
                    .set_doctype_identifiers(id, pubid.unwrap_or(""), sysid.unwrap_or(""))
                    .map_err(|_| "invalid doctype identifiers")
            });
        self.doctype_name = Some(name.to_string());
        self.flow(result)
    }

    fn start_element(&mut self, name: &str, attrs: &[Attribute], _specified: usize) -> Flow {
        let result = self.start(name, attrs);
        self.flow(result)
    }

    fn end_element(&mut self, _name: &str) -> Flow {
        let result = self.flush_text();
        if let Some((_, mark)) = self.open.pop() {
            self.bindings.truncate(mark);
        }
        self.flow(result)
    }

    fn chardata(&mut self, data: &str) -> Flow {
        if !self.open.is_empty() {
            self.text.push_str(data);
        }
        Flow::Continue
    }

    fn start_cdata(&mut self) -> Flow {
        let result = self.flush_text();
        self.flow(result)
    }

    fn end_cdata(&mut self) -> Flow {
        self.start_cdata()
    }

    fn comment(&mut self, data: &str) -> Flow {
        let result = self
            .add_node(
                NodeKind::Comment(data.to_string()),
                "invalid comment placement",
            )
            .map(|_| ());
        self.flow(result)
    }

    fn processing_instruction(&mut self, target: &str, data: &str) -> Flow {
        if target.eq_ignore_ascii_case("xml") {
            return self.fail("reserved processing instruction target");
        }
        let result = self
            .add_node(
                NodeKind::ProcessingInstruction {
                    target: target.to_string(),
                    data: data.trim().to_string(),
                },
                "invalid processing instruction",
            )
            .map(|_| ());
        self.flow(result)
    }

    fn skipped_entity(&mut self, _name: &str, _is_param: bool) -> Flow {
        self.fail("unknown entity reference")
    }

    fn external_entity_ref(
        &mut self,
        _context: Option<&str>,
        _base: Option<&str>,
        _sysid: Option<&str>,
        _pubid: Option<&str>,
    ) -> Flow {
        self.fail("external entities are not loaded")
    }
}

fn is_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|ch| ch != ':' && shared::is_name_start(ch))
        && chars.all(|ch| ch != ':' && shared::is_name_char(ch))
}

/// Split and validate an XML qualified name.
///
/// An unprefixed QName returns an empty prefix and the name as its local part.
pub fn split_qname(name: &str) -> Option<(&str, &str)> {
    if let Some((prefix, local)) = name.split_once(':') {
        (is_ncname(prefix) && is_ncname(local)).then_some((prefix, local))
    } else {
        is_ncname(name).then_some(("", name))
    }
}

fn namespace_for(bindings: &[(String, String)], prefix: &str) -> Option<String> {
    bindings
        .iter()
        .rev()
        .find(|(key, _)| key == prefix)
        .map(|(_, value)| value.clone())
}

fn bind_namespace(
    bindings: &mut Vec<(String, String)>,
    prefix: &str,
    uri: &str,
) -> Result<(), &'static str> {
    if prefix == "xmlns"
        || uri == XMLNS_NAMESPACE
        || (prefix == "xml") != (uri == XML_NAMESPACE)
        || (!prefix.is_empty() && uri.is_empty())
    {
        return Err("invalid namespace declaration");
    }
    bindings.push((String::from(prefix), String::from(uri)));
    Ok(())
}

fn namespace_from_uri(uri: Option<&str>) -> Namespace {
    match uri.unwrap_or("") {
        "http://www.w3.org/1999/xhtml" => Namespace::Html,
        "http://www.w3.org/2000/svg" => Namespace::Svg,
        "http://www.w3.org/1998/Math/MathML" => Namespace::MathMl,
        value => Namespace::Other(Rc::from(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_element(document: &Document) -> NodeId {
        document.first_child(document.root()).unwrap().unwrap()
    }

    #[test]
    fn xml_name_and_qname_validators_follow_distinct_colon_rules() {
        assert!(is_xml_name(":leading:colon"));
        assert!(is_xml_name("Élément_名"));
        assert!(!is_xml_name(""));
        assert!(!is_xml_name("9element"));
        assert!(!is_xml_name("bad name"));

        assert_eq!(split_qname("svg"), Some(("", "svg")));
        assert_eq!(split_qname("Élément:名"), Some(("Élément", "名")));
        assert!(split_qname(":svg").is_none());
        assert!(split_qname("svg:").is_none());
        assert!(split_qname("one:two:three").is_none());
        assert!(split_qname("9svg:circle").is_none());
    }

    #[test]
    fn resolves_default_and_prefixed_element_namespaces_without_case_folding() {
        let document = parse(
            "<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\" xmlns:x=\"urn:example\"><x:Thing x:code=\"v\" plain=\"p\"/></svg>",
            32,
        )
        .unwrap();
        let svg = root_element(&document);
        assert!(
            matches!(document.kind(svg), Ok(NodeKind::Element { namespace: Namespace::Svg, name, .. }) if name == "svg")
        );
        let child = document.first_child(svg).unwrap().unwrap();
        assert!(
            matches!(document.kind(child), Ok(NodeKind::Element { namespace: Namespace::Other(uri), name, .. }) if uri.as_ref() == "urn:example" && name == "x:Thing")
        );
        assert_eq!(
            document
                .get_attribute_ns(child, Some("urn:example"), "code")
                .unwrap()
                .as_deref(),
            Some("v")
        );
        assert_eq!(
            document
                .get_attribute_ns(child, None, "plain")
                .unwrap()
                .as_deref(),
            Some("p")
        );
        assert_eq!(
            document
                .attribute_namespace_uri(child, "x:code")
                .unwrap()
                .as_deref(),
            Some("urn:example")
        );

        let xhtml = parse(
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><body><MiXeD DATA-Code=\"case\"/></body></html>",
            32,
        ).unwrap();
        let html = root_element(&xhtml);
        let body = xhtml.first_child(html).unwrap().unwrap();
        let mixed = xhtml.first_child(body).unwrap().unwrap();
        for (node, expected_name) in [(html, "html"), (body, "body"), (mixed, "MiXeD")] {
            assert!(matches!(xhtml.kind(node),
                Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == expected_name));
        }
        assert_eq!(
            xhtml
                .get_attribute_ns(mixed, None, "DATA-Code")
                .unwrap()
                .as_deref(),
            Some("case")
        );
        assert_eq!(
            xhtml.get_attribute_ns(mixed, None, "data-code").unwrap(),
            None
        );
    }

    #[test]
    fn rejects_malformed_well_formedness_and_namespaces() {
        for source in [
            "<r a=\"1\"b=\"2\"/>",
            "<r a=\"<\"/>",
            "<r>text]]>text</r>",
            "<r>&#0;</r>",
            "<r xmlns:p=\"urn:x\" p:a=\"1\" q:a=\"2\" xmlns:q=\"urn:x\"/>",
            " <?xml version=\"1.0\"?><r/>",
            "<?xml nonsense?><r/>",
        ] {
            assert!(
                parse(source, 64).is_err(),
                "accepted malformed XML: {source}"
            );
        }
    }

    #[test]
    fn rejects_unknown_external_entities_without_loading_them() {
        assert!(parse(
            "<!DOCTYPE r [<!ENTITY e SYSTEM 'file:///etc/passwd'>]><r>&e;</r>",
            64,
        )
        .is_err());
    }

    #[test]
    fn namespace_attribute_prefix_replacement_keeps_single_mutation() {
        let mut document = Document::new(16);
        let element = document
            .create(NodeKind::Element {
                namespace: Namespace::Other(Rc::from("")),
                name: Name::new("root"),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(document.root(), element).unwrap();
        document
            .set_attribute_ns(element, Some("urn:keys"), "x:key", "old")
            .unwrap();
        document.clear_mutations();
        document
            .set_attribute_ns(element, Some("urn:keys"), "y:key", "new")
            .unwrap();
        assert_eq!(
            document
                .get_attribute_ns(element, Some("urn:keys"), "key")
                .unwrap()
                .as_deref(),
            Some("new")
        );
        assert_eq!(
            document.get_attribute_ns(element, None, "key").unwrap(),
            None
        );
        assert_eq!(document.mutations().len(), 1);
        assert!(matches!(
            document.mutations()[0].kind,
            crate::MutationKind::Attribute(_)
        ));
    }
}
