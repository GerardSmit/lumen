//! Bounded, non-validating XML parsing into the shared document arena.
//!
//! Tokenizing, well-formedness, character and entity references and DTD handling come from
//! `lumen_common::xml`; this module resolves namespaces and builds the tree. External subsets and
//! external entities are never loaded.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    rc::Rc,
    string::{String, ToString},
    vec,
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
    parse_initialized(input, max_nodes, |_| {})
}

/// Parse with document services installed before tokenization and tree construction.
/// Input size and character checks still precede document initialization.
pub fn parse_initialized(
    input: &str,
    max_nodes: usize,
    initialize: impl FnOnce(&mut Document),
) -> Result<Document, ParseError> {
    let input = checked_input(input)?;
    let mut document = Document::new(max_nodes);
    initialize(&mut document);
    let (parser, mut builder) = new_parser(document);
    run(parser, &mut builder, input)?;
    Ok(builder.document)
}

fn checked_input(input: &str) -> Result<&str, ParseError> {
    if input.len() > MAX_XML_BYTES {
        return Err(error(0, "XML input too large"));
    }
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    match input.char_indices().find(|(_, ch)| !shared::is_xml_char(*ch)) {
        Some((offset, _)) => Err(error(offset, "invalid XML character")),
        None => Ok(input),
    }
}

fn new_parser(document: Document) -> (Parser, Builder) {
    let parser = Parser::new(Options {
        encoding: Some(String::from("UTF-8")),
        ..Options::default()
    });
    let builder = Builder::new(parser.shared(), document);
    (parser, builder)
}

fn run(mut parser: Parser, builder: &mut Builder, input: &str) -> Result<(), ParseError> {
    match parser.parse(builder, input.as_bytes(), true) {
        Ok(()) => Ok(()),
        Err(failure) => Err(builder.failure.take().unwrap_or_else(|| {
            error(
                builder.shared.error_position().byte as usize,
                failure.message(),
            )
        })),
    }
}

const FRAGMENT_ROOT: &str = "lumen-fragment-root";

/// Parse a well-formed XML fragment using the namespace bindings in an element
/// context and move the completed fragment into the caller's document.
///
/// Parsing happens in a bounded temporary arena, so malformed input never
/// leaves partial nodes in the destination tree. The fragment is parsed as the
/// content of a wrapper element that declares the context's namespaces; input
/// that closes the wrapper early cannot balance the closing tag and is rejected.
pub fn parse_fragment_in(
    document: &mut Document,
    context: NodeId,
    input: &str,
) -> Result<NodeId, ParseError> {
    let input = checked_input(input)?;
    let (namespaces, html_catalog) = fragment_context(document, context)?;
    let mut wrapped = String::with_capacity(input.len() + 64);
    wrapped.push('<');
    wrapped.push_str(FRAGMENT_ROOT);
    for (prefix, uri) in namespaces.iter().skip(2) {
        wrapped.push_str(if prefix.is_empty() { " xmlns" } else { " xmlns:" });
        wrapped.push_str(prefix);
        wrapped.push_str("=\"");
        escape_attribute_value(&mut wrapped, uri);
        wrapped.push('"');
    }
    wrapped.push('>');
    let prefix_len = wrapped.len();
    wrapped.push_str(input);
    wrapped.push_str("</");
    wrapped.push_str(FRAGMENT_ROOT);
    wrapped.push('>');
    if wrapped.len() > MAX_XML_BYTES + prefix_len + FRAGMENT_ROOT.len() + 3 {
        return Err(error(0, "XML input too large"));
    }

    let remaining = document.max_nodes.saturating_sub(document.live_nodes);
    let (parser, mut builder) = new_parser(Document::new(remaining.saturating_add(2)));
    if html_catalog {
        parser
            .shared()
            .set_entity_catalog(Some(lumen_common::entities::semicolon));
    }
    run(parser, &mut builder, &wrapped).map_err(|failure| ParseError {
        offset: failure.offset.saturating_sub(prefix_len),
        ..failure
    })?;
    let mut temporary = builder.document;
    let root = temporary.root();
    let wrapper = temporary
        .first_child(root)
        .ok()
        .flatten()
        .filter(|&id| temporary.next_sibling(id).ok().flatten().is_none())
        .ok_or_else(|| error(0, "invalid XML fragment"))?;
    let fragment = temporary
        .create(NodeKind::DocumentFragment)
        .map_err(|_| error(0, "XML node limit exceeded"))?;
    let mut children = Vec::new();
    let mut child = temporary.first_child(wrapper).ok().flatten();
    while let Some(id) = child {
        children.push(id);
        child = temporary.next_sibling(id).ok().flatten();
    }
    for id in children {
        temporary
            .append(fragment, id)
            .map_err(|_| error(0, "invalid XML fragment placement"))?;
    }
    document
        .adopt_subtree_from(&mut temporary, fragment)
        .map(|(fragment, _)| fragment)
        .map_err(|error_kind| match error_kind {
            DomError::LimitExceeded => error(0, "XML node limit exceeded"),
            _ => error(0, "invalid XML fragment placement"),
        })
}

fn escape_attribute_value(out: &mut String, value: &str) {
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            '\t' => out.push_str("&#9;"),
            '\n' => out.push_str("&#10;"),
            '\r' => out.push_str("&#13;"),
            ch => out.push(ch),
        }
    }
}


fn fragment_context(
    document: &Document,
    context: NodeId,
) -> Result<(Vec<(String, String)>, bool), ParseError> {
    let mut ancestors = Vec::new();
    match document
        .kind(context)
        .map_err(|_| error(0, "invalid XML fragment context"))?
    {
        NodeKind::Element { .. } => {
            let mut current = Some(context);
            while let Some(id) = current {
                if ancestors.len() >= MAX_DEPTH {
                    return Err(error(0, "XML context nesting limit exceeded"));
                }
                if matches!(document.kind(id), Ok(NodeKind::Element { .. })) {
                    ancestors.push(id);
                }
                current = document
                    .parent(id)
                    .map_err(|_| error(0, "invalid XML fragment context"))?;
            }
            ancestors.reverse();
        }
        NodeKind::DocumentFragment => {}
        _ => {
            return Err(error(
                0,
                "XML fragment context must be an element or fragment",
            ))
        }
    }

    let mut namespaces = vec![
        (
            String::from("xml"),
            String::from("http://www.w3.org/XML/1998/namespace"),
        ),
        (
            String::from("xmlns"),
            String::from("http://www.w3.org/2000/xmlns/"),
        ),
    ];
    for &id in &ancestors {
        if let NodeKind::Element {
            attributes,
            name,
            namespace,
            ..
        } = document
            .kind(id)
            .map_err(|_| error(0, "invalid XML fragment context"))?
        {
            for (index, (attribute, value)) in attributes.iter().enumerate() {
                if document.attribute_namespace_uri_at(id, index)
                    != Some("http://www.w3.org/2000/xmlns/")
                {
                    continue;
                }
                let prefix = if attribute == "xmlns" {
                    ""
                } else if let Some(prefix) = attribute.strip_prefix("xmlns:") {
                    prefix
                } else {
                    continue;
                };
                bind_namespace(&mut namespaces, prefix, value).map_err(|message| error(0, message))?;
            }
            if id == context {
                let (prefix, _) = split_qname(name.as_str())
                    .ok_or_else(|| error(0, "invalid context qualified name"))?;
                if namespace_for(&namespaces, prefix).is_none() {
                    let uri = namespace_uri(namespace);
                    bind_namespace(&mut namespaces, prefix, uri).map_err(|message| error(0, message))?;
                }
            }
        }
    }

    let mut child = document
        .first_child(document.root())
        .map_err(|_| error(0, "invalid XML document"))?;
    let mut html_entity_catalog = false;
    while let Some(id) = child {
        if matches!(document.kind(id), Ok(NodeKind::DocumentType(_))) {
            let public_id = document
                .doctype_public_id(id)
                .map_err(|_| error(0, "invalid XML doctype"))?;
            html_entity_catalog = known_character_entity_catalog(public_id);
            break;
        }
        child = document
            .next_sibling(id)
            .map_err(|_| error(0, "invalid XML document"))?;
    }
    Ok((namespaces, html_entity_catalog))
}

fn namespace_uri(namespace: &Namespace) -> &str {
    match namespace {
        Namespace::Html => "http://www.w3.org/1999/xhtml",
        Namespace::Svg => "http://www.w3.org/2000/svg",
        Namespace::MathMl => "http://www.w3.org/1998/Math/MathML",
        Namespace::Other(uri) => uri,
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
    in_cdata: bool,
}

impl Builder {
    fn new(shared: Rc<Shared>, document: Document) -> Builder {
        Builder {
            shared,
            document,
            open: Vec::new(),
            bindings: alloc::vec![
                (String::from("xml"), String::from(XML_NAMESPACE)),
                (String::from("xmlns"), String::from(XMLNS_NAMESPACE)),
            ],
            text: String::new(),
            doctype_name: None,
            failure: None,
            in_cdata: false,
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
        if sysid.is_some() && pubid.is_some_and(known_character_entity_catalog) {
            self.shared
                .set_entity_catalog(Some(lumen_common::entities::semicolon));
        }
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
        self.in_cdata = true;
        self.flow(result)
    }

    fn end_cdata(&mut self) -> Flow {
        self.in_cdata = false;
        let text = core::mem::take(&mut self.text);
        let result = self
            .add(NodeKind::CData(text), "invalid CDATA placement")
            .map(|_| ());
        self.flow(result)
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

/// Whether `name` is a valid DOM attribute local name.
///
/// DOM attribute names are deliberately less restrictive than XML Names. They
/// may contain otherwise unusual punctuation, but cannot be empty or contain
/// ASCII whitespace, NULL, `/`, `=`, or `>`.
pub fn is_valid_attribute_local_name(name: &str) -> bool {
    !name.is_empty()
        && !name.chars().any(|character| {
            matches!(character, '\0' | '/' | '=' | '>')
                || matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' ')
        })
}

fn public_id_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b" \r\n-'()+,./:=?;!*#@$_%".contains(&byte)
}

fn known_character_entity_catalog(public_id: &str) -> bool {
    // HTML's XML parser catalog. The same named-reference table serves HTML
    // and these public DTD identifiers; no DTD text or entity map is allocated.
    const IDS: &[&str] = &[
        "-//W3C//DTD XHTML 1.0 Transitional//EN",
        "-//W3C//DTD XHTML 1.1//EN",
        "-//W3C//DTD XHTML 1.0 Strict//EN",
        "-//W3C//DTD XHTML 1.0 Frameset//EN",
        "-//W3C//DTD XHTML Basic 1.0//EN",
        "-//W3C//DTD XHTML 1.1 plus MathML 2.0//EN",
        "-//W3C//DTD XHTML 1.1 plus MathML 2.0 plus SVG 1.1//EN",
        "-//W3C//DTD MathML 2.0//EN",
        "-//WAPFORUM//DTD XHTML Mobile 1.0//EN",
        "-//WAPFORUM//DTD XHTML Mobile 1.1//EN",
        "-//WAPFORUM//DTD XHTML Mobile 1.2//EN",
    ];
    IDS.iter().any(|id| {
        public_id
            .split_ascii_whitespace()
            .eq(id.split_ascii_whitespace())
    })
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

enum SerializeEvent {
    Node {
        id: NodeId,
        context_namespace: Option<String>,
    },
    Sibling {
        id: NodeId,
        context_namespace: Option<String>,
    },
    Close {
        name: String,
        scope_start: usize,
    },
}


/// Serialize a node using XML fragment serialization and namespace fixup.
///
/// The namespace stack is restored as elements close, so a large sibling list
/// does not retain a copy of every ancestor's declarations. Output is bounded
/// independently of the document's node budget.
pub fn outer_html(document: &Document, root: NodeId) -> Result<String, DomError> {
    serialize_xml(document, root, true)
}

/// Serialize a node using the DOM Parsing XML serialization algorithm.
///
/// `require_well_formed` selects whether XML well-formedness constraints are
/// enforced. XMLSerializer uses `false`; XML `outerHTML` uses `true`.
pub fn serialize_xml(
    document: &Document,
    root: NodeId,
    require_well_formed: bool,
) -> Result<String, DomError> {
    if require_well_formed
        && matches!(document.kind(root)?, NodeKind::Document)
        && crate::selector::document_element(document).is_none()
    {
        return Err(DomError::WrongKind);
    }
    let mut output = String::new();
    let mut bindings = alloc::vec![(String::from("xml"), String::from(XML_NAMESPACE)),];
    let mut events = alloc::vec![SerializeEvent::Node {
        id: root,
        context_namespace: None,
    }];
    let mut next_prefix = 1usize;
    let mut open_elements = 0usize;

    while let Some(event) = events.pop() {
        match event {
            SerializeEvent::Close { name, scope_start } => {
                append_xml(&mut output, "</")?;
                append_xml(&mut output, &name)?;
                append_xml(&mut output, ">")?;
                bindings.truncate(scope_start);
                open_elements = open_elements.saturating_sub(1);
            }
            SerializeEvent::Sibling {
                id,
                context_namespace,
            } => {
                if let Some(next) = document.next_sibling(id)? {
                    events.push(SerializeEvent::Sibling {
                        id: next,
                        context_namespace: context_namespace.clone(),
                    });
                }
                events.push(SerializeEvent::Node {
                    id,
                    context_namespace,
                });
            }
            SerializeEvent::Node {
                id,
                context_namespace,
            } => {
                let kind = document.kind(id)?;
                match kind {
                    NodeKind::Document | NodeKind::DocumentFragment => {
                        if let Some(child) = document.first_child(id)? {
                            events.push(SerializeEvent::Sibling {
                                id: child,
                                context_namespace,
                            });
                        }
                    }
                    NodeKind::Attribute { .. } => {}
                    NodeKind::DocumentType(raw) => {
                        let name = stored_doctype_name(raw);
                        let public_id = document.doctype_public_id(id)?;
                        let system_id = document.doctype_system_id(id)?;
                        if require_well_formed
                            && (!is_xml_name(name)
                                || !public_id.bytes().all(public_id_char)
                                || !system_id.chars().all(shared::is_xml_char)
                                || (system_id.contains('"') && system_id.contains('\'')))
                        {
                            return Err(DomError::WrongKind);
                        }
                        append_xml(&mut output, "<!DOCTYPE ")?;
                        append_xml(&mut output, name)?;
                        if !public_id.is_empty() {
                            append_xml(&mut output, " PUBLIC \"")?;
                            append_xml(&mut output, public_id)?;
                            append_xml(&mut output, "\"")?;
                            if !system_id.is_empty() {
                                append_xml(&mut output, " \"")?;
                                append_xml(&mut output, system_id)?;
                                append_xml(&mut output, "\"")?;
                            }
                        } else if !system_id.is_empty() {
                            append_xml(&mut output, " SYSTEM \"")?;
                            append_xml(&mut output, system_id)?;
                            append_xml(&mut output, "\"")?;
                        }
                        append_xml(&mut output, ">")?;
                    }
                    NodeKind::Element {
                        namespace,
                        name,
                        attributes,
                    } => {
                        let attribute_bytes =
                            attributes
                                .iter()
                                .try_fold(name.len(), |total, (key, value)| {
                                    total
                                        .checked_add(key.len())
                                        .and_then(|size| size.checked_add(value.len()))
                                        .and_then(|size| size.checked_add(4))
                                        .filter(|size| *size <= MAX_XML_BYTES)
                                });
                        if attribute_bytes.is_none() {
                            return Err(DomError::LimitExceeded);
                        }
                        let scope_start = bindings.len();
                        let mut local_prefixes = Vec::new();
                        let mut local_default_namespace: Option<String> = None;

                        // Record XMLNS attributes in attribute order. The
                        // namespace map stores each URI's prefix history; a
                        // declaration does not erase an earlier URI mapping
                        // just because that prefix is rebound locally.
                        for (index, (attribute, value)) in attributes.iter().enumerate() {
                            if let Some(prefix) = declaration_prefix(
                                attribute.as_str(),
                                document.attribute_namespace_uri_at(id, index),
                            ) {
                                if require_well_formed && !valid_namespace_binding(prefix, value) {
                                    return Err(DomError::WrongKind);
                                }
                                if prefix.is_empty() {
                                    local_default_namespace = Some(value.clone());
                                } else {
                                    if value == XML_NAMESPACE {
                                        // The XML namespace is always serialized
                                        // with the reserved `xml` prefix.
                                        continue;
                                    }
                                    let mapped_uri = if value.is_empty() { "" } else { value };
                                    if !map_contains_prefix(&bindings, mapped_uri, prefix) {
                                        bindings
                                            .push((String::from(prefix), String::from(mapped_uri)));
                                        local_prefixes
                                            .push((String::from(prefix), String::from(mapped_uri)));
                                    }
                                }
                            }
                        }

                        let mut generated = Vec::new();
                        let mut generated_bytes = 0usize;
                        let mut ignore_default_declaration = false;
                        let (element_name, child_context_namespace) = fixup_element_name(
                            name.as_str(),
                            namespace_uri(namespace),
                            context_namespace.as_deref(),
                            local_default_namespace.as_deref(),
                            local_default_namespace.is_some(),
                            &mut local_prefixes,
                            &mut bindings,
                            &mut generated,
                            &mut generated_bytes,
                            &mut next_prefix,
                            &mut ignore_default_declaration,
                            require_well_formed,
                        )?;

                        append_xml(&mut output, "<")?;
                        append_xml(&mut output, &element_name)?;
                        for (prefix, uri) in &generated {
                            append_xml(&mut output, " xmlns")?;
                            if !prefix.is_empty() {
                                append_xml(&mut output, ":")?;
                                append_xml(&mut output, prefix)?;
                            }
                            append_xml(&mut output, "=\"")?;
                            escape_xml(&mut output, uri, true, require_well_formed)?;
                            append_xml(&mut output, "\"")?;
                        }
                        for (index, (name, value)) in attributes.iter().enumerate() {
                            let uri = document.attribute_namespace_uri_at(id, index);
                            let mut candidate = None;
                            if let Some(prefix) = declaration_prefix(name.as_str(), uri) {
                                if (ignore_default_declaration && prefix.is_empty())
                                    || (prefix.is_empty() && value == XML_NAMESPACE)
                                    || (!prefix.is_empty() && value == XML_NAMESPACE)
                                {
                                    continue;
                                }
                                if !prefix.is_empty()
                                    && !local_prefixes.iter().any(|(local, _)| local == prefix)
                                {
                                    continue;
                                }
                                if !prefix.is_empty()
                                    && local_prefixes
                                        .iter()
                                        .rev()
                                        .find(|(local, _)| local == prefix)
                                        .is_some_and(|(_, mapped)| mapped != value)
                                    && map_contains_prefix(&bindings, value, prefix)
                                {
                                    continue;
                                }
                                if require_well_formed
                                    && (value == XMLNS_NAMESPACE
                                        || (!prefix.is_empty() && value.is_empty()))
                                {
                                    return Err(DomError::WrongKind);
                                }
                            } else {
                                candidate = uri.and_then(|uri| {
                                    preferred_prefix(
                                        &bindings,
                                        uri,
                                        split_qname(name.as_str()).map(|(prefix, _)| prefix),
                                    )
                                });
                                if uri.is_some_and(|uri| uri == XMLNS_NAMESPACE) {
                                    candidate = Some(String::from("xmlns"));
                                } else if let Some(uri) = uri.filter(|uri| !uri.is_empty()) {
                                    if candidate.is_none() {
                                        let source_prefix = split_qname(name.as_str())
                                            .map(|(prefix, _)| prefix)
                                            .filter(|prefix| !prefix.is_empty());
                                        let prefix = if let Some(prefix) =
                                            source_prefix.filter(|prefix| {
                                                !local_prefixes
                                                    .iter()
                                                    .any(|(local, _)| local == *prefix)
                                            }) {
                                            String::from(prefix)
                                        } else {
                                            fresh_prefix(&mut next_prefix)
                                        };
                                        candidate = Some(prefix.clone());
                                        map_add(&mut bindings, uri, &prefix);
                                        local_prefixes.push((prefix.clone(), String::from(uri)));
                                        generated_bytes = generated_bytes
                                            .checked_add(prefix.len())
                                            .and_then(|bytes| bytes.checked_add(uri.len()))
                                            .and_then(|bytes| bytes.checked_add(8))
                                            .filter(|bytes| *bytes <= MAX_XML_BYTES)
                                            .ok_or(DomError::LimitExceeded)?;
                                        append_namespace_declaration(
                                            &mut output,
                                            &prefix,
                                            uri,
                                            require_well_formed,
                                        )?;
                                    }
                                }
                            }
                            append_xml(&mut output, " ")?;
                            append_fixed_attribute_name(
                                &mut output,
                                name.as_str(),
                                uri,
                                candidate.as_deref(),
                                &bindings,
                                require_well_formed,
                            )?;
                            append_xml(&mut output, "=\"")?;
                            escape_xml(&mut output, value, true, require_well_formed)?;
                            append_xml(&mut output, "\"")?;
                        }

                        let child_root = document.template_content(id)?.unwrap_or(id);
                        let child = document.first_child(child_root)?;
                        let has_children = child.is_some();
                        let is_html_void = *namespace == Namespace::Html
                            && crate::html::serializes_void(
                                split_qname(name.as_str())
                                    .map_or(name.as_str(), |(_, local)| local),
                            );
                        if is_html_void && !has_children {
                            append_xml(&mut output, " />")?;
                            bindings.truncate(scope_start);
                        } else if !has_children && !matches!(namespace, Namespace::Html) {
                            append_xml(&mut output, "/>")?;
                            bindings.truncate(scope_start);
                        } else {
                            if open_elements >= MAX_DEPTH {
                                return Err(DomError::LimitExceeded);
                            }
                            append_xml(&mut output, ">")?;
                            open_elements += 1;
                            events.push(SerializeEvent::Close {
                                name: element_name,
                                scope_start,
                            });
                            if let Some(child) = child {
                                events.push(SerializeEvent::Sibling {
                                    id: child,
                                    context_namespace: child_context_namespace,
                                });
                            }
                        }
                    }
                    NodeKind::Text(value) => {
                        escape_xml(&mut output, value, false, require_well_formed)?
                    }
                    NodeKind::CData(value) => {
                        append_xml(&mut output, "<![CDATA[")?;
                        append_cdata(&mut output, value, require_well_formed)?;
                        append_xml(&mut output, "]]>")?;
                    }
                    NodeKind::Comment(value) => {
                        if require_well_formed
                            && (validate_xml_serialized_data(value).is_err()
                                || value.contains("--")
                                || value.ends_with('-'))
                        {
                            return Err(DomError::WrongKind);
                        }
                        append_xml(&mut output, "<!--")?;
                        append_xml(&mut output, value)?;
                        append_xml(&mut output, "-->")?;
                    }
                    NodeKind::ProcessingInstruction { target, data } => {
                        if require_well_formed
                            && (validate_xml_serialized_data(target).is_err()
                                || validate_xml_serialized_data(data).is_err()
                                || !is_xml_name(target)
                                || target.contains(':')
                                || target.eq_ignore_ascii_case("xml")
                                || data.contains("?>"))
                        {
                            return Err(DomError::WrongKind);
                        }
                        append_xml(&mut output, "<?")?;
                        append_xml(&mut output, target)?;
                        append_xml(&mut output, " ")?;
                        append_xml(&mut output, data)?;
                        append_xml(&mut output, "?>")?;
                    }
                }
            }
        }
    }
    Ok(output)
}

fn stored_doctype_name(raw: &str) -> &str {
    let raw = raw.strip_prefix("<!DOCTYPE").unwrap_or(raw);
    let name = raw.trim_start_matches(char::is_whitespace);
    let end = name
        .find(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '[' | '>'))
        .unwrap_or(name.len());
    &name[..end]
}

fn declaration_prefix<'a>(name: &'a str, uri: Option<&str>) -> Option<&'a str> {
    if uri != Some(XMLNS_NAMESPACE) {
        return None;
    }
    if name == "xmlns" {
        Some("")
    } else {
        name.strip_prefix("xmlns:")
    }
}

fn valid_namespace_binding(prefix: &str, uri: &str) -> bool {
    prefix != "xmlns"
        && uri != XMLNS_NAMESPACE
        && (prefix == "xml") == (uri == XML_NAMESPACE)
        && (prefix.is_empty() || !uri.is_empty())
}

fn bind_for_serialization(
    prefix: &str,
    uri: &str,
    bindings: &mut Vec<(String, String)>,
    generated: &mut Vec<(String, String)>,
    generated_bytes: &mut usize,
) -> Result<(), DomError> {
    let added = prefix
        .len()
        .checked_add(uri.len())
        .and_then(|bytes| bytes.checked_add(8))
        .ok_or(DomError::LimitExceeded)?;
    *generated_bytes = generated_bytes
        .checked_add(added)
        .filter(|bytes| *bytes <= MAX_XML_BYTES)
        .ok_or(DomError::LimitExceeded)?;
    if !prefix.is_empty() {
        map_add(bindings, uri, prefix);
    }
    generated.push((String::from(prefix), String::from(uri)));
    Ok(())
}

fn existing_prefix(bindings: &[(String, String)], uri: &str) -> Option<String> {
    bindings
        .iter()
        .rev()
        .find_map(|(prefix, bound_uri)| (bound_uri == uri).then(|| prefix.clone()))
}

fn preferred_prefix(
    bindings: &[(String, String)],
    uri: &str,
    requested: Option<&str>,
) -> Option<String> {
    if let Some(requested) = requested {
        if map_contains_prefix(bindings, uri, requested) {
            return Some(String::from(requested));
        }
    }
    existing_prefix(bindings, uri)
}

fn map_contains_prefix(bindings: &[(String, String)], uri: &str, prefix: &str) -> bool {
    bindings
        .iter()
        .any(|(candidate, mapped_uri)| candidate == prefix && mapped_uri == uri)
}

fn map_add(bindings: &mut Vec<(String, String)>, uri: &str, prefix: &str) {
    if !map_contains_prefix(bindings, uri, prefix) {
        bindings.push((String::from(prefix), String::from(uri)));
    }
}

fn fresh_prefix(next: &mut usize) -> String {
    let candidate = alloc::format!("ns{}", *next);
    *next = (*next).saturating_add(1);
    candidate
}

fn fixup_element_name(
    qname: &str,
    uri: &str,
    context_namespace: Option<&str>,
    local_default_namespace: Option<&str>,
    has_local_default_namespace: bool,
    local_prefixes: &mut Vec<(String, String)>,
    bindings: &mut Vec<(String, String)>,
    generated: &mut Vec<(String, String)>,
    generated_bytes: &mut usize,
    next_prefix: &mut usize,
    ignore_default_declaration: &mut bool,
    require_well_formed: bool,
) -> Result<(String, Option<String>), DomError> {
    let (prefix, local) = match split_qname(qname) {
        Some(parts) => parts,
        None if !require_well_formed => ("", qname),
        None => return Err(DomError::WrongKind),
    };
    let namespace = (!uri.is_empty()).then_some(uri);
    let parent_namespace = context_namespace.filter(|uri| !uri.is_empty());
    if parent_namespace == namespace {
        if has_local_default_namespace {
            *ignore_default_declaration = true;
        }
        let name = if namespace == Some(XML_NAMESPACE) {
            alloc::format!("xml:{local}")
        } else {
            String::from(local)
        };
        return Ok((name, parent_namespace.map(String::from)));
    }

    let mut candidate = preferred_prefix(bindings, uri, (!prefix.is_empty()).then_some(prefix));
    if prefix == "xmlns" {
        if require_well_formed {
            return Err(DomError::WrongKind);
        }
        candidate = Some(String::from(prefix));
    }
    if let Some(candidate) = candidate {
        let inherited =
            if has_local_default_namespace && local_default_namespace != Some(XML_NAMESPACE) {
                local_default_namespace
                    .filter(|namespace| !namespace.is_empty())
                    .map(String::from)
            } else {
                parent_namespace.map(String::from)
            };
        return Ok((alloc::format!("{candidate}:{local}"), inherited));
    }

    if !prefix.is_empty() {
        let generated_prefix = if local_prefixes
            .iter()
            .any(|(local_prefix, _)| local_prefix == prefix)
        {
            fresh_prefix(next_prefix)
        } else {
            String::from(prefix)
        };
        map_add(bindings, uri, &generated_prefix);
        local_prefixes.push((generated_prefix.clone(), String::from(uri)));
        bind_for_serialization(&generated_prefix, uri, bindings, generated, generated_bytes)?;
        let inherited =
            if has_local_default_namespace && local_default_namespace != Some(XML_NAMESPACE) {
                local_default_namespace
                    .filter(|namespace| !namespace.is_empty())
                    .map(String::from)
            } else {
                parent_namespace.map(String::from)
            };
        return Ok((alloc::format!("{generated_prefix}:{local}"), inherited));
    }

    if !has_local_default_namespace || local_default_namespace != namespace {
        *ignore_default_declaration = true;
        bind_for_serialization("", uri, bindings, generated, generated_bytes)?;
        return Ok((String::from(local), namespace.map(String::from)));
    }

    Ok((String::from(local), namespace.map(String::from)))
}

fn append_fixed_attribute_name(
    output: &mut String,
    qname: &str,
    uri: Option<&str>,
    candidate_prefix: Option<&str>,
    _bindings: &[(String, String)],
    require_well_formed: bool,
) -> Result<(), DomError> {
    // Namespace declaration names are already in their required lexical
    // form. In particular, the default declaration `xmlns` has no prefix;
    // treating its XMLNS namespace like an ordinary namespaced attribute
    // would incorrectly serialize it as `xmlns:xmlns`.
    if declaration_prefix(qname, uri).is_some() {
        if require_well_formed && !is_xml_name(qname) {
            return Err(DomError::WrongKind);
        }
        return append_xml(output, qname);
    }
    let local = split_qname(qname).map_or(qname, |(_, local)| local);
    let Some(uri) = uri.filter(|uri| !uri.is_empty()) else {
        if require_well_formed && (!is_xml_name(local) || local.contains(':') || local == "xmlns") {
            return Err(DomError::WrongKind);
        }
        return append_xml(output, local);
    };
    if require_well_formed && (!is_xml_name(local) || local.contains(':')) {
        return Err(DomError::WrongKind);
    }
    let prefix = candidate_prefix.ok_or(DomError::WrongKind)?;
    append_xml(output, prefix)?;
    append_xml(output, ":")?;
    append_xml(output, local)
}

fn append_namespace_declaration(
    output: &mut String,
    prefix: &str,
    uri: &str,
    require_well_formed: bool,
) -> Result<(), DomError> {
    if require_well_formed && !valid_namespace_binding(prefix, uri) {
        return Err(DomError::WrongKind);
    }
    append_xml(output, " xmlns")?;
    if !prefix.is_empty() {
        append_xml(output, ":")?;
        append_xml(output, prefix)?;
    }
    append_xml(output, "=\"")?;
    escape_xml(output, uri, true, require_well_formed)?;
    append_xml(output, "\"")
}

fn append_xml(output: &mut String, text: &str) -> Result<(), DomError> {
    lumen_common::limits::size::append_string(output, text, MAX_XML_BYTES)
        .map_err(|_| DomError::LimitExceeded)
}

fn validate_xml_serialized_data(input: &str) -> Result<(), DomError> {
    if input.chars().all(shared::is_xml_char) {
        Ok(())
    } else {
        Err(DomError::WrongKind)
    }
}

fn escape_xml(
    output: &mut String,
    input: &str,
    attribute: bool,
    require_well_formed: bool,
) -> Result<(), DomError> {
    let mut run = 0;
    for (index, ch) in input.char_indices() {
        if require_well_formed && !shared::is_xml_char(ch) {
            return Err(DomError::WrongKind);
        }
        let replacement = match ch {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '"' if attribute => Some("&quot;"),
            '\r' if attribute => Some("&#13;"),
            '\n' if attribute => Some("&#10;"),
            '\t' if attribute => Some("&#9;"),
            _ => None,
        };
        if let Some(replacement) = replacement {
            append_xml(output, &input[run..index])?;
            append_xml(output, replacement)?;
            run = index + ch.len_utf8();
        }
    }
    append_xml(output, &input[run..])
}
fn append_cdata(
    output: &mut String,
    value: &str,
    require_well_formed: bool,
) -> Result<(), DomError> {
    if require_well_formed && value.chars().any(|ch| !shared::is_xml_char(ch)) {
        return Err(DomError::WrongKind);
    }
    let mut remaining = value;
    while let Some(index) = remaining.find("]]>") {
        append_xml(output, &remaining[..index + 2])?;
        append_xml(output, "]]><![CDATA[>")?;
        remaining = &remaining[index + 3..];
    }
    append_xml(output, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_element(document: &Document) -> NodeId {
        let mut child = document.first_child(document.root()).unwrap();
        while let Some(node) = child {
            if matches!(document.kind(node).unwrap(), NodeKind::Element { .. }) {
                return node;
            }
            child = document.next_sibling(node).unwrap();
        }
        panic!("parsed document has no root element")
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

    #[test]
    fn initialized_xml_parser_observes_initial_xhtml_details_transitions() {
        use core::cell::RefCell;
        let events = Rc::new(RefCell::new(Vec::new()));
        let capture = events.clone();
        let mut calls = 0;
        let document = parse_initialized(
            "<root xmlns:h='http://www.w3.org/1999/xhtml'><h:details name='g' open=''/><h:details name='g' open=''/></root>",
            32,
            |document| {
                calls += 1;
                assert!(document.first_child(document.root()).unwrap().is_none());
                document.set_details_transition_sink(Some(Rc::new(move |_, event| capture.borrow_mut().push(event))));
            },
        ).unwrap();
        assert_eq!(calls, 1);
        let root = root_element(&document);
        let first = document.first_child(root).unwrap().unwrap();
        let second = document.next_sibling(first).unwrap().unwrap();
        assert!(matches!(document.kind(first).unwrap(), NodeKind::Element { namespace: Namespace::Html, name, .. } if crate::svg::local_name(name) == "details"));
        assert_eq!(*events.borrow(), vec![
            crate::details::DetailsTransition { node: first, old_open: false, new_open: true },
            crate::details::DetailsTransition { node: second, old_open: false, new_open: true },
            crate::details::DetailsTransition { node: second, old_open: true, new_open: false },
        ]);
        assert_eq!(document.details_open_state(second), Some(false));
    }

    #[test]
    fn initialized_xml_parser_preserves_early_checks_and_parse_errors() {
        let too_large = "a".repeat(MAX_XML_BYTES + 1);
        for input in [too_large.as_str(), "<root>\u{0000}</root>"] {
            let mut calls = 0;
            let actual = parse_initialized(input, 32, |_| calls += 1).err().unwrap();
            assert_eq!(calls, 0);
            assert_eq!(actual, parse(input, 32).err().unwrap());
        }
        for (input, max_nodes) in [("<root><child></root>", 32), ("<root><child/></root>", 1)] {
            let mut calls = 0;
            let actual = parse_initialized(input, max_nodes, |_| calls += 1).err().unwrap();
            assert_eq!(calls, 1);
            assert_eq!(actual, parse(input, max_nodes).err().unwrap());
        }
    }

    #[test]
    fn xml_html_templates_have_detached_contents_and_preserve_namespace_scopes() {
        let mut document = parse(
            "<root xmlns='urn:default' xmlns:h='http://www.w3.org/1999/xhtml'><h:template xmlns=''><g/><h:template><nested/></h:template>text<!--note--><![CDATA[data]]></h:template><sibling/></root>",
            32,
        ).unwrap();
        let root = root_element(&document);
        let template = document.first_child(root).unwrap().unwrap();
        let content = document.template_content(template).unwrap().expect("prefixed HTML template content");
        assert_eq!(document.parent(content).unwrap(), None);
        assert_eq!(document.first_child(template).unwrap(), None);
        let g = document.first_child(content).unwrap().unwrap();
        assert!(matches!(document.kind(g).unwrap(), NodeKind::Element { namespace: Namespace::Other(uri), name, .. } if uri.is_empty() && name == "g"));
        let nested_template = document.next_sibling(g).unwrap().unwrap();
        let nested_content = document.template_content(nested_template).unwrap().unwrap();
        assert_eq!(document.first_child(nested_template).unwrap(), None);
        assert!(document.first_child(nested_content).unwrap().is_some());
        let text = document.next_sibling(nested_template).unwrap().unwrap();
        assert!(matches!(document.kind(text).unwrap(), NodeKind::Text(value) if value == "text"));
        let comment = document.next_sibling(text).unwrap().unwrap();
        assert!(matches!(document.kind(comment).unwrap(), NodeKind::Comment(value) if value == "note"));
        let cdata = document.next_sibling(comment).unwrap().unwrap();
        assert!(matches!(document.kind(cdata).unwrap(), NodeKind::CData(value) if value == "data"));
        let sibling = document.next_sibling(template).unwrap().unwrap();
        assert!(matches!(document.kind(sibling).unwrap(), NodeKind::Element { namespace: Namespace::Other(uri), .. } if uri.as_ref() == "urn:default"));
        let clone = document.clone_subtree(template).unwrap();
        assert!(document.first_child(document.template_content(clone).unwrap().unwrap()).unwrap().is_some());
        let fragment = parse_fragment_in(&mut document, root, "<h:template xmlns=''><g/></h:template><sibling/>").unwrap();
        let fragment_template = document.first_child(fragment).unwrap().unwrap();
        assert!(document.first_child(document.template_content(fragment_template).unwrap().unwrap()).unwrap().is_some());
        assert_eq!(document.first_child(fragment_template).unwrap(), None);
    }

    #[test]
    fn xml_template_creation_respects_namespace_case_and_fragment_node_budget() {
        for (markup, is_template) in [
            ("<h:template xmlns:h='http://www.w3.org/1999/xhtml'/>", true),
            ("<template xmlns='http://www.w3.org/1999/xhtml'/>", true),
            ("<h:Template xmlns:h='http://www.w3.org/1999/xhtml'/>", false),
            ("<h:template xmlns:h='urn:foreign'/>", false),
        ] {
            let document = parse(markup, 3).unwrap();
            let root = root_element(&document);
            assert_eq!(document.template_content(root).unwrap().is_some(), is_template);
            assert_eq!(document.node_count(), if is_template { 3 } else { 2 });
            assert_eq!(parse(markup, 2).is_err(), is_template);
        }
    }

    #[test]
    fn xml_fragment_parser_inherits_namespaces_and_accepts_multiple_nodes() {
        let mut document = parse(
            "<root xmlns='urn:default' xmlns:p='urn:prefixed'><context/></root>",
            32,
        )
        .unwrap();
        let root = root_element(&document);
        let context = document.first_child(root).unwrap().unwrap();
        let fragment = parse_fragment_in(
            &mut document,
            context,
            "<plain/><p:item>text</p:item>tail<!--note--><?ready yes?>",
        )
        .unwrap();
        let plain = document.first_child(fragment).unwrap().unwrap();
        let prefixed = document.next_sibling(plain).unwrap().unwrap();
        let text = document.next_sibling(prefixed).unwrap().unwrap();
        let comment = document.next_sibling(text).unwrap().unwrap();
        let instruction = document.next_sibling(comment).unwrap().unwrap();
        assert!(matches!(
            document.kind(plain).unwrap(),
            NodeKind::Element {
                namespace: Namespace::Other(uri), name, ..
            } if uri.as_ref() == "urn:default" && name == "plain"
        ));
        assert!(matches!(
            document.kind(prefixed).unwrap(),
            NodeKind::Element {
                namespace: Namespace::Other(uri), name, ..
            } if uri.as_ref() == "urn:prefixed" && name == "p:item"
        ));
        assert_eq!(document.kind(text).unwrap(), &NodeKind::Text("tail".into()));
        assert_eq!(
            document.kind(comment).unwrap(),
            &NodeKind::Comment("note".into())
        );
        assert!(matches!(
            document.kind(instruction).unwrap(),
            NodeKind::ProcessingInstruction { target, data }
                if target == "ready" && data == "yes"
        ));
        assert_eq!(document.next_sibling(instruction).unwrap(), None);
    }

    #[test]
    fn malformed_or_over_budget_xml_fragments_leave_the_document_unchanged() {
        let mut document = parse("<root><kept/></root>", 8).unwrap();
        let root = root_element(&document);
        let kept = document.first_child(root).unwrap().unwrap();
        let before = document.node_count();
        assert!(parse_fragment_in(&mut document, root, "<new><broken></new>").is_err());
        assert_eq!(document.node_count(), before);
        assert_eq!(document.first_child(root).unwrap(), Some(kept));

        let mut small = parse("<root/>", 4).unwrap();
        let small_root = root_element(&small);
        let before = small.node_count();
        assert!(parse_fragment_in(&mut small, small_root, "<a/><b/>").is_err());
        assert_eq!(small.node_count(), before);
        assert_eq!(
            small.first_child(small_root).unwrap(),
            None,
            "failed fragment parsing must not attach partial children"
        );

        let oversized =
            alloc::string::String::from_utf8(alloc::vec![b'x'; MAX_XML_BYTES + 1]).unwrap();
        assert!(parse_fragment_in(&mut small, small_root, &oversized).is_err());
        assert_eq!(small.node_count(), before);
    }

    #[test]
    fn xml_outer_html_preserves_namespace_fixup_and_escapes_markup() {
        let document = parse(
            "<root xmlns='urn:default' xmlns:p='urn:prefixed'><p:child p:flag='x&amp;y'>a&amp;b<![CDATA[c]]></p:child><h:area xmlns:h='http://www.w3.org/1999/xhtml'/></root>",
            32,
        )
        .unwrap();
        let root = root_element(&document);
        let child = document.first_child(root).unwrap().unwrap();
        let area = document.next_sibling(child).unwrap().unwrap();
        assert_eq!(
            outer_html(&document, child).unwrap(),
            "<p:child xmlns:p=\"urn:prefixed\" p:flag=\"x&amp;y\">a&amp;b<![CDATA[c]]></p:child>"
        );
        assert_eq!(
            outer_html(&document, area).unwrap(),
            "<h:area xmlns:h=\"http://www.w3.org/1999/xhtml\" />"
        );
        assert_eq!(
            outer_html(&document, root).unwrap(),
            "<root xmlns=\"urn:default\" xmlns:p=\"urn:prefixed\"><p:child p:flag=\"x&amp;y\">a&amp;b<![CDATA[c]]></p:child><h:area xmlns:h=\"http://www.w3.org/1999/xhtml\" /></root>"
        );

        let standalone = parse("<plain/>", 4).unwrap();
        let standalone_root = root_element(&standalone);
        assert_eq!(
            outer_html(&standalone, standalone_root).unwrap(),
            "<plain/>"
        );

        let nested = parse("<outer xmlns='urn:outer'><plain xmlns=''/></outer>", 8).unwrap();
        let nested_root = root_element(&nested);
        assert_eq!(
            outer_html(&nested, nested_root).unwrap(),
            "<outer xmlns=\"urn:outer\"><plain xmlns=\"\"/></outer>"
        );
    }

    #[test]
    fn xml_outer_html_rejects_nodes_that_cannot_be_serialized_well_formed() {
        let mut document = parse("<root/>", 8).unwrap();
        let root = root_element(&document);
        let comment = document
            .create(NodeKind::Comment(String::from("bad--comment")))
            .unwrap();
        document.append(root, comment).unwrap();
        assert_eq!(outer_html(&document, root), Err(DomError::WrongKind));
        document.remove(comment).unwrap();

        let text = document
            .create(NodeKind::Text(String::from("\u{0}")))
            .unwrap();
        document.append(root, text).unwrap();
        assert_eq!(outer_html(&document, root), Err(DomError::WrongKind));
    }

    #[test]
    fn xml_serializer_mode_is_lenient_while_outer_html_remains_strict() {
        let mut document = parse("<root/>", 12).unwrap();
        let root = root_element(&document);
        let comment = document
            .create(NodeKind::Comment(String::from("bad--comment")))
            .unwrap();
        document.append(root, comment).unwrap();
        let text = document
            .create(NodeKind::Text(String::from("\u{0}")))
            .unwrap();
        document.append(root, text).unwrap();

        assert_eq!(
            serialize_xml(&document, root, false).unwrap(),
            "<root><!--bad--comment-->\u{0}</root>"
        );
        assert_eq!(
            serialize_xml(&document, root, true),
            Err(DomError::WrongKind)
        );
        assert_eq!(outer_html(&document, root), Err(DomError::WrongKind));
    }

    #[test]
    fn xml_serializer_non_wellformed_mode_keeps_invalid_local_names() {
        let mut document = parse("<root/>", 8).unwrap();
        let root = root_element(&document);
        let child = document
            .create(NodeKind::Element {
                namespace: Namespace::Other(Rc::from("")),
                name: Name::new("bad name"),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(root, child).unwrap();
        assert_eq!(
            serialize_xml(&document, root, false).unwrap(),
            "<root><bad name/></root>"
        );
        assert_eq!(
            serialize_xml(&document, root, true),
            Err(DomError::WrongKind)
        );
    }

    #[test]
    fn xml_serializer_uses_scoped_prefix_map_and_unbound_attribute_prefixes() {
        let mut shadowed = parse(
            "<el1 xmlns:p='u1' xmlns:q='u1'><el2 xmlns:q='u2'/></el1>",
            8,
        )
        .unwrap();
        let root = root_element(&shadowed);
        let child = shadowed.first_child(root).unwrap().unwrap();
        shadowed
            .set_attribute_ns(child, Some("u1"), "name", "v")
            .unwrap();
        assert_eq!(
            serialize_xml(&shadowed, root, false).unwrap(),
            "<el1 xmlns:p=\"u1\" xmlns:q=\"u1\"><el2 xmlns:q=\"u2\" q:name=\"v\"/></el1>"
        );

        let mut prefixed = parse("<root/>", 8).unwrap();
        let root = root_element(&prefixed);
        prefixed
            .set_attribute_ns(root, Some("http://www.w3.org/1999/xlink"), "xl:type", "v")
            .unwrap();
        assert_eq!(
            serialize_xml(&prefixed, root, false).unwrap(),
            "<root xmlns:xl=\"http://www.w3.org/1999/xlink\" xl:type=\"v\"/>"
        );

        let mut generated =
            parse("<root xmlns:ns2='uri2'><child xmlns:ns1='uri1'/></root>", 8).unwrap();
        let root = root_element(&generated);
        let child = generated.first_child(root).unwrap().unwrap();
        generated
            .set_attribute_ns(child, Some("uri3"), "attr1", "value1")
            .unwrap();
        assert_eq!(
            serialize_xml(&generated, root, false).unwrap(),
            "<root xmlns:ns2=\"uri2\"><child xmlns:ns1=\"uri1\" xmlns:ns1=\"uri3\" ns1:attr1=\"value1\"/></root>"
        );
    }

    #[test]
    fn xml_catalog_entities_reuse_html_table_in_text_and_attributes() {
        let document = parse(
            "<!DOCTYPE html PUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN' 'http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd'><html xmlns='http://www.w3.org/1999/xhtml' title='&copy;&NotEqualTilde;'>&nbsp;&NotEqualTilde;<![CDATA[&nbsp;]]></html>",
            16,
        ).unwrap();
        let doctype = document.first_child(document.root()).unwrap().unwrap();
        let html = document.next_sibling(doctype).unwrap().unwrap();
        assert_eq!(
            document
                .get_attribute_ns(html, None, "title")
                .unwrap()
                .as_deref(),
            Some("\u{a9}\u{2242}\u{338}")
        );
        let text = document.first_child(html).unwrap().unwrap();
        assert_eq!(
            document.kind(text).unwrap(),
            &NodeKind::Text("\u{a0}\u{2242}\u{338}".into())
        );
        let cdata = document.next_sibling(text).unwrap().unwrap();
        assert_eq!(
            document.kind(cdata).unwrap(),
            &NodeKind::CData("&nbsp;".into())
        );
        assert!(known_character_entity_catalog(
            "-//W3C//DTD  XHTML\n1.0 Strict//EN"
        ));
    }

    #[test]
    fn xml_catalog_does_not_enable_unknown_external_or_internal_entities() {
        for source in [
            "<r>&nbsp;</r>",
            "<!DOCTYPE r SYSTEM 'http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd'><r>&nbsp;</r>",
            "<!DOCTYPE r PUBLIC '-//OTHER//DTD Example//EN' 'file:///example.dtd'><r>&nbsp;</r>",
            "<!DOCTYPE html PUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN'><html/>",
            "<!DOCTYPE html PUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN' 'ignored'><html>&notit;</html>",
            "<!DOCTYPE html PUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN' 'ignored'><html>&nbsp</html>",
        ] {
            assert!(
                parse(source, 16).is_err(),
                "unexpected entity/doctype acceptance: {source}"
            );
        }
    }

    #[test]
    fn internal_general_entities_expand_nested_text_attributes_and_markup() {
        let source = "<!DOCTYPE foo [<!ENTITY who \"world\"><!ENTITY greeting \"hello &who;\"><!ENTITY rich \"<b title='&greeting;'>&greeting;</b>tail\"><!ENTITY encoded \"&lt;i/&gt;\"><!ENTITY numeric \"&#60;i/>\"><!ENTITY ampersand \"&#38;lt;\">]><foo title='&greeting;'>A&greeting;:&rich;&encoded;&numeric;&ampersand;Z</foo>";
        let document = parse(source, 32).unwrap();
        let doctype = document.first_child(document.root()).unwrap().unwrap();
        assert!(matches!(
            document.kind(doctype),
            Ok(NodeKind::DocumentType(name)) if name == "foo"
        ));
        let foo = document.next_sibling(doctype).unwrap().unwrap();
        assert_eq!(
            document
                .get_attribute_ns(foo, None, "title")
                .unwrap()
                .as_deref(),
            Some("hello world")
        );

        let leading = document.first_child(foo).unwrap().unwrap();
        assert_eq!(
            document.kind(leading).unwrap(),
            &NodeKind::Text("Ahello world:".into())
        );
        let bold = document.next_sibling(leading).unwrap().unwrap();
        assert!(matches!(
            document.kind(bold),
            Ok(NodeKind::Element { name, .. }) if name == "b"
        ));
        assert_eq!(
            document
                .get_attribute_ns(bold, None, "title")
                .unwrap()
                .as_deref(),
            Some("hello world")
        );
        let bold_text = document.first_child(bold).unwrap().unwrap();
        assert_eq!(
            document.kind(bold_text).unwrap(),
            &NodeKind::Text("hello world".into())
        );
        let trailing = document.next_sibling(bold).unwrap().unwrap();
        assert_eq!(
            document.kind(trailing).unwrap(),
            &NodeKind::Text("tail<i/><i/>&lt;Z".into())
        );

        // Entity declarations are parser input only; the DOM doctype writer
        // serializes the identifiers and name without copying the subset.
        assert_eq!(
            serialize_xml(&document, document.root(), false).unwrap(),
            "<!DOCTYPE foo><foo title=\"hello world\">Ahello world:<b title=\"hello world\">hello world</b>tail&lt;i/&gt;&lt;i/&gt;&amp;lt;Z</foo>"
        );
    }

    #[test]
    fn internal_entity_attribute_references_follow_xml_less_than_rules() {
        let document = parse(
            "<!DOCTYPE r [<!ENTITY safe '&lt;'><!ENTITY nested '&safe;'>]><r value='&nested;'/>",
            16,
        )
        .unwrap();
        let root = root_element(&document);
        assert_eq!(
            document
                .get_attribute_ns(root, None, "value")
                .unwrap()
                .as_deref(),
            Some("<")
        );

        for source in [
            "<!DOCTYPE r [<!ENTITY less '<'>]><r value='&less;'/>",
            "<!DOCTYPE r [<!ENTITY less '&#60;'>]><r value='&less;'/>",
            "<!DOCTYPE r [<!ENTITY less '<'><!ENTITY nested '&less;'>]><r value='&nested;'/>",
        ] {
            assert!(
                parse(source, 16).is_err(),
                "accepted less-than from replacement text: {source}"
            );
        }
    }

    #[test]
    fn internal_entities_override_catalog_and_external_entities_are_never_loaded() {
        let document = parse(
            "<!DOCTYPE html PUBLIC '-//W3C//DTD XHTML 1.0 Strict//EN' 'ignored' [<!ENTITY nbsp 'replacement'>]><html>&nbsp;</html>",
            16,
        )
        .unwrap();
        let doctype = document.first_child(document.root()).unwrap().unwrap();
        let html = document.next_sibling(doctype).unwrap().unwrap();
        let text = document.first_child(html).unwrap().unwrap();
        assert_eq!(
            document.kind(text).unwrap(),
            &NodeKind::Text("replacement".into())
        );

        assert!(
            parse(
                "<!DOCTYPE r [<!ENTITY remote SYSTEM 'file:///etc/passwd'>]><r/>",
                16,
            )
            .is_ok(),
            "an unused external entity declaration is not fetched"
        );
        assert!(
            parse(
                "<!DOCTYPE r [<!ENTITY remote SYSTEM 'file:///etc/passwd'>]><r>&remote;</r>",
                16,
            )
            .is_err(),
            "an external entity reference is not fetched"
        );
    }

    #[test]
    fn dom_attribute_local_names_use_the_less_restrictive_dom_grammar() {
        for name in [
            "x",
            ":",
            "0",
            "0:a",
            "x:y:x",
            "invalid^Name",
            "\\",
            "'",
            "\"",
            "~",
        ] {
            assert!(is_valid_attribute_local_name(name), "{name:?}");
        }
        for name in [
            "", "bad name", "a\tb", "a\nb", "a\u{c}b", "a\rb", "a\0b", "a/b", "a=b", "a>b",
        ] {
            assert!(!is_valid_attribute_local_name(name), "{name:?}");
        }
    }

    #[test]
    fn xml_parser_preserves_cdata_sections_including_empty_sections() {
        let document = parse(
            "<root><![CDATA[]]><item><![CDATA[x<y&z]]></item></root>",
            16,
        )
        .unwrap();
        let root = root_element(&document);
        let empty = document.first_child(root).unwrap().unwrap();
        assert_eq!(
            document.kind(empty).unwrap(),
            &NodeKind::CData(String::new())
        );
        let item = document.next_sibling(empty).unwrap().unwrap();
        let cdata = document.first_child(item).unwrap().unwrap();
        assert_eq!(
            document.kind(cdata).unwrap(),
            &NodeKind::CData("x<y&z".into())
        );
        assert_eq!(document.character_data_length(cdata), Ok(5));
    }
}
