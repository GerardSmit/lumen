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

fn tree_failure(failure:DomError,context:&'static str)->&'static str {
    if failure==DomError::LimitExceeded {"XML DOM allocation limit exceeded"}else {context}
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
    let (parser, mut builder) = new_parser(&mut document);
    run(parser, &mut builder, input)?;
    drop(builder);
    Ok(document)
}

fn checked_input(input: &str) -> Result<&str, ParseError> {
    if input.len() > MAX_XML_BYTES {
        return Err(error(0, "XML input too large"));
    }
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    match input
        .char_indices()
        .find(|(_, ch)| !shared::is_xml_char(*ch))
    {
        Some((offset, _)) => Err(error(offset, "invalid XML character")),
        None => Ok(input),
    }
}

fn new_parser(document: &mut Document) -> (Parser, Builder<'_,Document>) {
    let parser = Parser::new(Options {
        encoding: Some(String::from("UTF-8")),
        ..Options::default()
    });
    let builder = Builder::new(parser.shared(), document);
    (parser, builder)
}

fn run(mut parser: Parser, builder: &mut Builder<'_,Document>, input: &str) -> Result<(), ParseError> {
    match parser.parse(builder, input.as_bytes(), true) {
        Ok(()) => Ok(()),
        Err(failure) => {
            let original=builder.state.failure.take().unwrap_or_else(|| {
            error(
                builder.shared.error_position().byte as usize,
                failure.message(),
            )
        });
            let _=builder.retire_open_elements();
            Err(original)
        }
    }
}

const FRAGMENT_ROOT: &str = "lumen-fragment-root";

/// Resumable XML tree construction over the host's live Document. Namespace
/// and tokenizer state survive each script turn; DOMParser uses the same
/// builder with scripting yields disabled.
pub struct XmlDocumentParser {
    parser: Parser,
    state: Option<BuilderState>,
    closed: bool,
    paused_script: Option<NodeId>,
}

impl XmlDocumentParser {
    pub fn start(document: &mut Document, input: &str) -> Result<(Self, Option<NodeId>), ParseError> {
        let input = checked_input(input)?;
        let (mut parser, mut builder) = new_parser(document);
        builder.state.yield_scripts = true;
        let result = parser.parse(&mut builder, input.as_bytes(), true);
        if let Err(failure) = result {
            let original=builder.state.failure.take().unwrap_or_else(|| error(builder.shared.error_position().byte as usize, failure.message()));
            let _=builder.retire_open_elements();
            return Err(original);
        }
        let script = builder.state.yielded_script.take();
        let closed = !parser.is_suspended();
        Ok((Self { parser, state: Some(builder.state), closed, paused_script: script }, script))
    }

    pub fn resume<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &mut D) -> Result<Option<NodeId>, ParseError> {
        if self.closed { return Ok(None); }
        let mut builder = Builder { shared: self.parser.shared(), document,
            state: self.state.take().expect("XML parser state available between feeds") };
        let result = self.parser.resume(&mut builder);
        let script = builder.state.yielded_script.take();
        let failure = builder.state.failure.take();
        let offset = builder.shared.error_position().byte as usize;
        if result.is_err() {let _=builder.retire_open_elements();}
        self.state = Some(builder.state);
        self.project_retained_nodes(|node|document.current_node(node));
        if let Err(error_code) = result {
            self.closed=true;self.paused_script=None;
            // A fatal well-formedness failure ends open-element status; XML
            // does not synthesize end tags or recover the tokenizer stream.
            if let Some(state)=&mut self.state {state.pending_element=None;state.yielded_script=None;}
            return Err(failure.unwrap_or_else(|| error(offset, error_code.message())));
        }
        self.closed = !self.parser.is_suspended();
        let script=script.map(|node|document.current_node(node));
        self.paused_script = script;
        Ok(script)
    }

    fn retire_after_tree_error<D:crate::parser_documents::ParserDocument+?Sized>(&mut self,document:&mut D) {
        self.closed=true;self.paused_script=None;
        if let Some(state)=self.state.take() {
            let mut builder=Builder {shared:self.parser.shared(),document,state};
            let _=builder.retire_open_elements();self.state=Some(builder.state);
        }
        self.project_retained_nodes(|node|document.current_node(node));
    }

    pub fn has_open_element(&self,node:NodeId)->bool {
        self.state.as_ref().is_some_and(|state|state.open.iter().any(|open|open.element==node))
    }

    pub fn visit_retained_nodes(&self, mut visit: impl FnMut(NodeId)) {
        if let Some(script) = self.paused_script { visit(script); }
        if let Some(state) = &self.state {
            if let Some(pending) = &state.pending_element { visit(pending.node); visit(pending.parent); }
            for open in &state.open { visit(open.element); visit(open.content); }
            if let Some(script) = state.yielded_script { visit(script); }
        }
    }

    pub fn project_retained_nodes(&mut self,mut project:impl FnMut(NodeId)->NodeId) {
        self.paused_script=self.paused_script.map(&mut project);
        if let Some(state)=&mut self.state {state.project_nodes(&mut project);}
    }

    pub fn pending_element(&self) -> Option<NodeId> {
        self.state.as_ref()?.pending_element.as_ref().map(|pending| pending.node)
    }

    pub fn pending_element_context(&self) -> Option<NodeId> {
        self.state.as_ref()?.pending_element.as_ref().map(|pending| pending.parent)
    }

    pub fn replace_pending_element<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &D, replacement: NodeId) -> Result<(), ParseError> {
        let content=document.template_content(replacement).map_err(|_|error(0,"invalid XML template"))?.unwrap_or(replacement);
        let state = self.state.as_mut().expect("XML parser state between feeds");
        let old = state.pending_element.as_ref().expect("pending XML element").node;
        state.pending_element.as_mut().unwrap().node = replacement;
        for open in &mut state.open {
            if open.element == old {
                open.element = replacement;
                open.content = content;
            }
        }
        if self.paused_script == Some(old) { self.paused_script = Some(replacement); }
        Ok(())
    }

    pub fn append_pending_element_attributes<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &mut D) -> Result<(), ParseError> {
        let result=(|| {
            let pending = self.state.as_mut().unwrap().pending_element.as_mut().expect("pending XML element");
            for (name, value, uri) in core::mem::take(&mut pending.attributes) {
                document.set_attribute_ns(pending.node, uri.as_deref(), name.as_str(), &value)
                    .map_err(|failure|error(0,tree_failure(failure,"invalid XML token attribute")))?;
            }
            Ok(())
        })();
        if result.is_err() {self.retire_after_tree_error(document);}
        result
    }

    pub fn insert_pending_element<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &mut D) -> Result<(), ParseError> {
        let pending = self.state.as_mut().unwrap().pending_element.take().expect("pending XML element");
        let result=document.insert_before(pending.parent, pending.node, None).map_err(|failure|error(0,tree_failure(failure,"invalid XML element placement")));
        if result.is_err() {self.retire_after_tree_error(document);}
        self.project_retained_nodes(|node|document.current_node(node));
        result
    }

    pub fn resume_after_pending_element<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &mut D) -> Result<Option<NodeId>, ParseError> {
        if let Some(script) = self.paused_script { Ok(Some(script)) } else { self.resume(document) }
    }

    pub fn finish_pending_element<D:crate::parser_documents::ParserDocument+?Sized>(&mut self, document: &mut D) -> Result<Option<NodeId>, ParseError> {
        self.insert_pending_element(document)?;
        self.resume_after_pending_element(document)
    }
}

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
    for (index, (prefix, uri)) in namespaces.iter().enumerate().skip(2) {
        if namespaces[index + 1..].iter().any(|(later, _)| later == prefix) {
            continue;
        }
        wrapped.push_str(if prefix.is_empty() {
            " xmlns"
        } else {
            " xmlns:"
        });
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
    let mut temporary = Document::new(remaining.saturating_add(2));
    let (parser, mut builder) = new_parser(&mut temporary);
    if html_catalog {
        parser
            .shared()
            .set_entity_catalog(Some(lumen_common::entities::semicolon));
    }
    run(parser, &mut builder, &wrapped).map_err(|failure| ParseError {
        offset: failure.offset.saturating_sub(prefix_len),
        ..failure
    })?;
    drop(builder);
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
                bind_namespace(&mut namespaces, prefix, value)
                    .map_err(|message| error(0, message))?;
            }
            if id == context {
                let (prefix, _) = split_qname(name.as_str())
                    .ok_or_else(|| error(0, "invalid context qualified name"))?;
                if namespace_for(&namespaces, prefix).is_none() {
                    let uri = namespace_uri(namespace);
                    bind_namespace(&mut namespaces, prefix, uri)
                        .map_err(|message| error(0, message))?;
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

pub(crate) fn namespace_uri(namespace: &Namespace) -> &str {
    match namespace {
        Namespace::Html => "http://www.w3.org/1999/xhtml",
        Namespace::Svg => "http://www.w3.org/2000/svg",
        Namespace::MathMl => "http://www.w3.org/1998/Math/MathML",
        Namespace::Other(uri) => uri,
    }
}

struct Open {
    element: NodeId,
    content: NodeId,
    mark: usize,
}

struct Builder<'a,D:crate::parser_documents::ParserDocument+?Sized> {
    shared: Rc<Shared>,
    document: &'a mut D,
    state: BuilderState,
}

struct BuilderState {
    open: Vec<Open>,
    bindings: Vec<(String, String)>,
    text: String,
    doctype_name: Option<String>,
    failure: Option<ParseError>,
    in_cdata: bool,
    in_doctype: bool,
    yield_scripts: bool,
    yielded_script: Option<NodeId>,
    pending_element: Option<PendingXmlElement>,
}

impl BuilderState {
    fn project_nodes(&mut self,mut project:impl FnMut(NodeId)->NodeId) {
        for open in &mut self.open {open.element=project(open.element);open.content=project(open.content);}
        self.yielded_script=self.yielded_script.map(&mut project);
        if let Some(pending)=&mut self.pending_element {pending.node=project(pending.node);pending.parent=project(pending.parent);}
    }
}

struct PendingXmlElement {
    node: NodeId,
    parent: NodeId,
    attributes: Vec<(Name, String, Option<String>)>,
}

impl<'a,D:crate::parser_documents::ParserDocument+?Sized> Builder<'a,D> {
    fn finish_style(&self,node:NodeId)->Result<(),&'static str> {
        if matches!(self.document.kind(node),Ok(NodeKind::Element {namespace:Namespace::Html|Namespace::Svg,name,..})
            if name.as_str().rsplit(':').next()==Some("style")) {
            self.document.record_parser_style_block_update(node).map_err(|failure|tree_failure(failure,"invalid style block update"))?;
        }
        Ok(())
    }

    fn retire_open_elements(&mut self)->Result<(),&'static str> {
        let mut failure=if self.state.in_cdata {
            let text=core::mem::take(&mut self.state.text);
            if text.is_empty() {None}else {self.add(NodeKind::CData(text),"invalid CDATA placement").err()}
        }else {self.flush_text().err()};
        // Fatal XML detection ends open-element status without synthetic end
        // tags or script preparation. Style retirement uses the actual owner.
        while let Some(open)=self.state.open.pop() {
            self.state.bindings.truncate(open.mark);
            self.document.record_parser_element_completion(open.element);
            if let Err(error)=self.finish_style(open.element) {failure.get_or_insert(error);}
        }
        self.state.pending_element=None;self.state.yielded_script=None;
        failure.map_or(Ok(()),Err)
    }

    fn new(shared: Rc<Shared>, document: &'a mut D) -> Builder<'a,D> {
        Builder {
            shared,
            document,
            state: BuilderState {
            open: Vec::new(),
            bindings: alloc::vec![
                (String::from("xml"), String::from(XML_NAMESPACE)),
                (String::from("xmlns"), String::from(XMLNS_NAMESPACE)),
            ],
            text: String::new(),
            doctype_name: None,
            failure: None,
            in_cdata: false,
            in_doctype: false,
            yield_scripts: false,
            yielded_script: None,
            pending_element: None,
            },
        }
    }

    fn fail(&mut self, message: &'static str) -> Flow {
        let offset = self.shared.position().byte as usize;
        self.state.failure.get_or_insert(error(offset, message));
        Flow::Abort
    }

    fn parent(&self) -> NodeId {
        self.state.open
            .last()
            .map_or(self.document.root(), |open| open.content)
    }

    fn add(&mut self, kind: NodeKind, placement: &'static str) -> Result<NodeId, &'static str> {
        let parent = self.parent();
        let id = self.document.create_at(parent,kind,None).map_err(|e| match e {
            DomError::LimitExceeded => "XML node limit exceeded",
            _ => "invalid XML node",
        })?;
        self.document
            .append(parent, id)
            .map_err(|failure|tree_failure(failure,placement))?;
        Ok(id)
    }

    fn flush_text(&mut self) -> Result<(), &'static str> {
        if self.state.text.is_empty() {
            return Ok(());
        }
        let text = core::mem::take(&mut self.state.text);
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
        if self.state.open.len() >= MAX_DEPTH {
            return Err("XML nesting limit exceeded");
        }
        if self.state.open.is_empty()
            && self
                .state.doctype_name
                .as_deref()
                .is_some_and(|name| name != qname)
        {
            return Err("doctype name does not match document element");
        }
        let mark = self.state.bindings.len();
        for attr in attrs {
            if attr.name == "xmlns" {
                bind_namespace(&mut self.state.bindings, "", &attr.value)?;
            } else if let Some(prefix) = attr.name.strip_prefix("xmlns:") {
                if prefix.is_empty() || split_qname(prefix).is_none_or(|(p, _)| !p.is_empty()) {
                    return Err("invalid namespace prefix");
                }
                bind_namespace(&mut self.state.bindings, prefix, &attr.value)?;
            }
        }
        let (prefix, local) = split_qname(qname).ok_or("invalid qualified name")?;
        let uri = if prefix.is_empty() {
            namespace_for(&self.state.bindings, "")
        } else {
            Some(namespace_for(&self.state.bindings, prefix).ok_or("unbound element prefix")?)
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
                Some(namespace_for(&self.state.bindings, prefix).ok_or("unbound attribute prefix")?)
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
        let namespace = namespace_from_uri(uri.as_deref());
        let is_value = attrs.iter().find(|attribute| attribute.name == "is").map(|attribute| attribute.value.as_str());
        let parent = self.parent();
        let deferred = self.state.yield_scripts && namespace == Namespace::Html
            && self.document.parser_custom_element_defined(parent, local, is_value);
        let kind = NodeKind::Element {
            namespace,
            name: Name::new(qname),
            attributes: if deferred { Vec::new() } else { attrs
                .iter()
                .map(|attr| (Name::new(&attr.name), attr.value.clone()))
                .collect() },
        };
        let id = self.document.create_at(parent,kind, is_value).map_err(|e| match e {
            DomError::LimitExceeded => "XML node limit exceeded",
            _ => "invalid XML node",
        })?;
        self.document.record_parser_element_birth(id,parent,true).map_err(|failure|tree_failure(failure,"invalid XML registry association"))?;
        if deferred {
            let attributes = attrs.iter().map(|attribute| {
                let uri = namespace_metadata.iter().find(|(name, _)| *name == attribute.name)
                    .and_then(|(_, uri)| uri.clone());
                (Name::new(&attribute.name), attribute.value.clone(), uri)
            }).collect();
            self.state.pending_element = Some(PendingXmlElement { node: id, parent, attributes });
            self.shared.suspend();
        } else {
        for (qualified_name, uri) in namespace_metadata {
            self.document
                .set_attribute_namespace_metadata(id, qualified_name, uri.as_deref())
                .map_err(|failure|tree_failure(failure,"invalid attribute namespace metadata"))?;
        }
        self.document
            .append(parent, id)
            .map_err(|failure|tree_failure(failure,"invalid element placement"))?;
        }
        // HTML templates keep their children in a detached content fragment, even
        // when the qualified name carries a namespace prefix.
        let content = self
            .document
            .template_content(id)
            .map_err(|failure|tree_failure(failure,"invalid template contents"))?
            .unwrap_or(id);
        self.state.open.push(Open { element: id, content, mark });
        Ok(())
    }
}

impl<D:crate::parser_documents::ParserDocument+?Sized> Handler for Builder<'_,D> {
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
                    .map_err(|failure|tree_failure(failure,"invalid doctype identifiers"))
            });
        self.state.doctype_name = Some(name.to_string());
        self.state.in_doctype = true;
        self.flow(result)
    }
    fn end_doctype(&mut self)->Flow {self.state.in_doctype=false;Flow::Continue}

    fn start_element(&mut self, name: &str, attrs: &[Attribute], _specified: usize) -> Flow {
        let result = self.start(name, attrs);
        self.flow(result)
    }

    fn end_element(&mut self, _name: &str) -> Flow {
        let mut result = self.flush_text();
        if let Some(open) = self.state.open.pop() {
            self.state.bindings.truncate(open.mark);
            self.document.record_parser_element_completion(open.element);
            if result.is_ok() {result=self.finish_style(open.element);}
            if result.is_ok() && self.state.yield_scripts && matches!(
                self.document.kind(open.element),
                Ok(NodeKind::Element { namespace: Namespace::Html | Namespace::Svg, name, .. })
                    if name.as_str().rsplit(':').next() == Some("script")
            ) {
                self.state.yielded_script = Some(open.element);
                self.shared.suspend();
            }
        }
        self.flow(result)
    }

    fn chardata(&mut self, data: &str) -> Flow {
        if !self.state.open.is_empty() {
            self.state.text.push_str(data);
        }
        Flow::Continue
    }

    fn start_cdata(&mut self) -> Flow {
        let result = self.flush_text();
        self.state.in_cdata = true;
        self.flow(result)
    }

    fn end_cdata(&mut self) -> Flow {
        self.state.in_cdata = false;
        let text = core::mem::take(&mut self.state.text);
        let result = self
            .add(NodeKind::CData(text), "invalid CDATA placement")
            .map(|_| ());
        self.flow(result)
    }

    fn comment(&mut self, data: &str) -> Flow {
        if self.state.in_doctype {return Flow::Continue;}
        let result = self
            .add_node(
                NodeKind::Comment(data.to_string()),
                "invalid comment placement",
            )
            .map(|_| ());
        self.flow(result)
    }

    fn processing_instruction(&mut self, target: &str, data: &str) -> Flow {
        // Internal-subset PIs are not direct Document children. The browser
        // DOM has no DTD child tree; CSSOM only considers actual prolog nodes.
        if self.state.in_doctype {return Flow::Continue;}
        if target.eq_ignore_ascii_case("xml") {
            return self.fail("reserved processing instruction target");
        }
        let result = self
            .add_node(
                NodeKind::ProcessingInstruction {
                    target: target.to_string(),
                    // The shared scanner has already consumed the required
                    // separator whitespace after the target. Any remaining
                    // whitespace belongs to the PI data and must be retained.
                    data: data.to_string(),
                },
                "invalid processing instruction",
            )
            .and_then(|node| self.document.record_parser_style_block_update(node).map_err(|failure|tree_failure(failure,"XML stylesheet instruction admission")));
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
            matches!(character, '\0' | '/' | '=' | '>') || is_ascii_whitespace(character)
        })
}

/// The DOM qualified-name validation context from DOM §1.4.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomNameContext {
    Element,
    Attribute,
}

/// The two exception classes produced by DOM's namespace/name validation algorithm.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DomNameError {
    InvalidCharacter,
    Namespace,
}

/// The validated components of a DOM qualified name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DomQualifiedName<'namespace, 'name> {
    pub namespace: Option<&'namespace str>,
    pub prefix: Option<&'name str>,
    pub local_name: &'name str,
}

/// Whether `name` satisfies DOM's current element-local-name grammar.
///
/// Names beginning with ASCII alpha use the HTML parser's permissive character
/// set. Other names use the restricted historical ASCII set with non-ASCII
/// code points accepted.
pub fn is_valid_element_local_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first.is_ascii_alphabetic() {
        return !name.chars().any(|character| {
            is_ascii_whitespace(character) || matches!(character, '\0' | '/' | '>')
        });
    }
    if !matches!(first, ':' | '_') && first < '\u{80}' {
        return false;
    }
    chars.all(|character| {
        character >= '\u{80}'
            || character.is_ascii_alphanumeric()
            || matches!(character, '-' | '.' | ':' | '_')
    })
}

/// Whether `name` satisfies DOM's doctype-name grammar. The empty string is valid.
pub fn is_valid_doctype_name(name: &str) -> bool {
    !name
        .chars()
        .any(|character| is_ascii_whitespace(character) || matches!(character, '\0' | '>'))
}

fn is_valid_namespace_prefix(prefix: &str) -> bool {
    !prefix.is_empty()
        && !prefix.chars().any(|character| {
            is_ascii_whitespace(character) || matches!(character, '\0' | '/' | '>')
        })
}

fn is_ascii_whitespace(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' ')
}

/// Validate and split a DOM qualified name, preserving the current DOM name
/// grammar rather than XML's stricter QName grammar.
pub fn validate_dom_qualified_name<'namespace, 'name>(
    namespace: Option<&'namespace str>,
    qualified_name: &'name str,
    context: DomNameContext,
) -> Result<DomQualifiedName<'namespace, 'name>, DomNameError> {
    let namespace = namespace.filter(|namespace| !namespace.is_empty());
    let (prefix, local_name) = if let Some((prefix, local_name)) = qualified_name.split_once(':') {
        if !is_valid_namespace_prefix(prefix) {
            return Err(DomNameError::InvalidCharacter);
        }
        (Some(prefix), local_name)
    } else {
        (None, qualified_name)
    };

    let valid_local_name = match context {
        DomNameContext::Element => is_valid_element_local_name(local_name),
        DomNameContext::Attribute => is_valid_attribute_local_name(local_name),
    };
    if !valid_local_name {
        return Err(DomNameError::InvalidCharacter);
    }

    if (prefix.is_some() && namespace.is_none())
        || (prefix == Some("xml") && namespace != Some(XML_NAMESPACE))
        || ((qualified_name == "xmlns" || prefix == Some("xmlns"))
            && namespace != Some(XMLNS_NAMESPACE))
        || (namespace == Some(XMLNS_NAMESPACE)
            && qualified_name != "xmlns"
            && prefix != Some("xmlns"))
    {
        return Err(DomNameError::Namespace);
    }

    Ok(DomQualifiedName {
        namespace,
        prefix,
        local_name,
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

pub(crate) fn namespace_from_uri(uri: Option<&str>) -> Namespace {
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
pub fn outer_html(document: &Document, root: NodeId) -> Result<String, DomError> { outer_html_with_graph(document, root) }

pub fn outer_html_with_graph<G: crate::graph::DocumentGraph>(graph: &G, root: NodeId) -> Result<String, DomError> { serialize_xml_with_graph(graph, root, true) }

/// Serialize ordinary children as a well-formed XML fragment. Template contents live in
/// their separate fragment, and each top-level child starts with a fresh namespace scope.
pub fn inner_html(document: &Document, root: NodeId) -> Result<String, DomError> { inner_html_with_graph(document, root) }

pub fn inner_html_with_graph<G: crate::graph::DocumentGraph>(graph: &G, root: NodeId) -> Result<String, DomError> {
    let root = graph.read(root)?.template_content(root)?.unwrap_or(root);
    serialize_xml_start(graph, root, true, true)
}

/// Serialize a node using the DOM Parsing XML serialization algorithm.
///
/// `require_well_formed` selects whether XML well-formedness constraints are
/// enforced. XMLSerializer uses `false`; XML `outerHTML` uses `true`.
pub fn serialize_xml(document: &Document, root: NodeId, require_well_formed: bool) -> Result<String, DomError> { serialize_xml_with_graph(document, root, require_well_formed) }

pub fn serialize_xml_with_graph<G: crate::graph::DocumentGraph>(graph: &G, root: NodeId, require_well_formed: bool) -> Result<String, DomError> { serialize_xml_start(graph, root, require_well_formed, false) }

fn serialize_xml_start<G: crate::graph::DocumentGraph>(
    graph: &G,
    root: NodeId,
    require_well_formed: bool,
    children_only: bool,
) -> Result<String, DomError> {
    let root_document = graph.read(root)?;
    let document = &*root_document;
    if require_well_formed
        && !children_only
        && matches!(document.kind(root)?, NodeKind::Document)
        && crate::selector::document_element(document).is_none()
    {
        return Err(DomError::WrongKind);
    }
    let mut output = String::new();
    let mut bindings = alloc::vec![(String::from("xml"), String::from(XML_NAMESPACE)),];
    let mut events = Vec::new();
    events.try_reserve_exact(1).map_err(|_| DomError::LimitExceeded)?;
    if children_only {
        if let Some(child) = document.first_child(root)? {
            events.push(SerializeEvent::Sibling { id: child, context_namespace: None });
        }
    } else {
        events.push(SerializeEvent::Node { id: root, context_namespace: None });
    }
    let mut next_prefix = 1usize;
    let mut open_elements = 0usize;

    while let Some(event) = events.pop() {
        let event_node = match &event { SerializeEvent::Node { id, .. } | SerializeEvent::Sibling { id, .. } => Some(*id), SerializeEvent::Close { .. } => None };
        let event_document = event_node.map(|id| graph.read(id)).transpose()?;
        let document = event_document.as_deref().unwrap_or(&*root_document);
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
                        let (element_prefix, element_local_name) =
                            document.element_name_parts(id)?;
                        let (element_name, child_context_namespace) = fixup_element_name(
                            element_prefix,
                            element_local_name,
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
                                let (attribute_prefix, _) =
                                    attribute_name_parts(name.as_str(), uri);
                                candidate = uri.and_then(|uri| {
                                    preferred_prefix(&bindings, uri, attribute_prefix)
                                });
                                if uri.is_some_and(|uri| uri == XMLNS_NAMESPACE) {
                                    candidate = Some(String::from("xmlns"));
                                } else if let Some(uri) = uri.filter(|uri| !uri.is_empty()) {
                                    if candidate.is_none() {
                                        // DOM Parsing prefers an authored prefix
                                        // that is not declared on this element.
                                        // Ancestor bindings remain in the namespace
                                        // history, but the local-prefix map is the
                                        // conflict check for attributes.
                                        let prefix = attribute_prefix
                                            .filter(|prefix| {
                                                !prefix.is_empty()
                                                    && !local_prefixes.iter().any(
                                                        |(local, _)| local == prefix,
                                                    )
                                            })
                                            .map(String::from)
                                            .unwrap_or_else(|| fresh_prefix(&mut next_prefix));
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
                                attribute_name_parts(name.as_str(), uri),
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
                        let child = graph.read(child_root)?.first_child(child_root)?;
                        let has_children = child.is_some();
                        let is_html_void = *namespace == Namespace::Html
                            && crate::html::serializes_void(element_local_name);
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

fn attribute_name_parts<'a>(name: &'a str, uri: Option<&str>) -> (Option<&'a str>, &'a str) {
    if uri.is_none() {
        // setAttribute() creates a null-namespace Attr whose entire qualified
        // name is its local name, even when it contains a colon.
        return (None, name);
    }
    if uri == Some(XMLNS_NAMESPACE) {
        if name == "xmlns" {
            return (None, name);
        }
        if let Some(local_name) = name.strip_prefix("xmlns:") {
            return (Some("xmlns"), local_name);
        }
    }
    match name.split_once(':') {
        Some((prefix, local_name)) => (Some(prefix), local_name),
        None => (None, name),
    }
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
    prefix: Option<&str>,
    local_name: &str,
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
    if require_well_formed
        && (!is_ncname(local_name) || prefix.is_some_and(|prefix| !is_ncname(prefix)))
    {
        return Err(DomError::WrongKind);
    }
    let namespace = (!uri.is_empty()).then_some(uri);
    let parent_namespace = context_namespace.filter(|uri| !uri.is_empty());
    if parent_namespace == namespace {
        if has_local_default_namespace {
            *ignore_default_declaration = true;
        }
        let name = if namespace == Some(XML_NAMESPACE) {
            alloc::format!("xml:{local_name}")
        } else {
            String::from(local_name)
        };
        return Ok((name, parent_namespace.map(String::from)));
    }

    let mut candidate =
        preferred_prefix(bindings, uri, prefix.filter(|prefix| !prefix.is_empty()));
    if prefix == Some("xmlns") {
        if require_well_formed {
            return Err(DomError::WrongKind);
        }
        candidate = Some(String::from("xmlns"));
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
        return Ok((alloc::format!("{candidate}:{local_name}"), inherited));
    }

    if let Some(prefix) = prefix.filter(|prefix| !prefix.is_empty()) {
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
        return Ok((alloc::format!("{generated_prefix}:{local_name}"), inherited));
    }

    if !has_local_default_namespace || local_default_namespace != namespace {
        *ignore_default_declaration = true;
        bind_for_serialization("", uri, bindings, generated, generated_bytes)?;
        return Ok((String::from(local_name), namespace.map(String::from)));
    }

    Ok((String::from(local_name), namespace.map(String::from)))
}

fn append_fixed_attribute_name(
    output: &mut String,
    qname: &str,
    (prefix, local_name): (Option<&str>, &str),
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
        if require_well_formed
            && (prefix.is_some_and(|prefix| !is_ncname(prefix)) || !is_ncname(local_name))
        {
            return Err(DomError::WrongKind);
        }
        return append_xml(output, qname);
    }
    if uri.is_none_or(str::is_empty) {
        if require_well_formed && (!is_ncname(local_name) || local_name == "xmlns") {
            return Err(DomError::WrongKind);
        }
        return append_xml(output, local_name);
    }
    if require_well_formed
        && (!is_ncname(local_name) || prefix.is_some_and(|prefix| !is_ncname(prefix)))
    {
        return Err(DomError::WrongKind);
    }
    let prefix = candidate_prefix.ok_or(DomError::WrongKind)?;
    append_xml(output, prefix)?;
    append_xml(output, ":")?;
    append_xml(output, local_name)
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

    #[test]
    fn specification_live_xml_parser_element_creation_phases() {
        let mut document = Document::new(64);
        document.set_parser_custom_element_predicate(Some(Rc::new(|_, _, local, _| local == "x-phase")));
        let (mut parser, script) = XmlDocumentParser::start(&mut document,
            "<html xmlns=\"http://www.w3.org/1999/xhtml\"><x-phase data-value=\"token\"><span>child</span></x-phase><script>after</script></html>").unwrap();
        assert!(script.is_none());
        let node = parser.pending_element().expect("XML constructor phase");
        assert_eq!(document.parent(node), Ok(None));
        assert_eq!(document.first_child(node), Ok(None));
        assert_eq!(document.get_attribute_ns_ref(node, None, "data-value"), Ok(None));
        parser.append_pending_element_attributes(&mut document).unwrap();
        assert_eq!(document.get_attribute_ns_ref(node, None, "data-value").unwrap(), Some("token"));
        assert!(parser.finish_pending_element(&mut document).unwrap().is_some());
        assert!(document.parent(node).unwrap().is_some());
        assert!(document.first_child(node).unwrap().is_some());
        assert!(parser.resume(&mut document).unwrap().is_none());
    }

    #[test]
    fn specification_live_xml_parser_entity_script_continuations() {
        let mut document = Document::new(64);
        let (mut parser, first) = XmlDocumentParser::start(&mut document,
            r#"<!DOCTYPE html [<!ENTITY inner "<script>first</script><p>after</p>"><!ENTITY outer "&inner;<span>end</span>">]><html xmlns="http://www.w3.org/1999/xhtml">&outer;<script>second</script></html>"#).unwrap();
        let first = first.expect("entity script yields");
        let root = document.parent(first).unwrap().unwrap();
        assert_eq!(document.next_sibling(first), Ok(None));
        let second = parser.resume(&mut document).unwrap().expect("document script yields");
        assert_ne!(first, second);
        assert_eq!(document.parent(second), Ok(Some(root)));
        let p = document.next_sibling(first).unwrap().unwrap();
        assert!(matches!(document.kind(p), Ok(NodeKind::Element { name, .. }) if name == "p"));
        assert!(parser.resume(&mut document).unwrap().is_none());
    }

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
        assert!(
            matches!(document.kind(first).unwrap(), NodeKind::Element { namespace: Namespace::Html, name, .. } if crate::svg::local_name(name) == "details")
        );
        assert_eq!(
            *events.borrow(),
            vec![
                crate::details::DetailsTransition {
                    node: first,
                    old_open: false,
                    new_open: true
                },
                crate::details::DetailsTransition {
                    node: second,
                    old_open: false,
                    new_open: true
                },
                crate::details::DetailsTransition {
                    node: second,
                    old_open: true,
                    new_open: false
                },
            ]
        );
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
            let actual = parse_initialized(input, max_nodes, |_| calls += 1)
                .err()
                .unwrap();
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
        let content = document
            .template_content(template)
            .unwrap()
            .expect("prefixed HTML template content");
        assert_eq!(document.parent(content).unwrap(), None);
        assert_eq!(document.first_child(template).unwrap(), None);
        let g = document.first_child(content).unwrap().unwrap();
        assert!(
            matches!(document.kind(g).unwrap(), NodeKind::Element { namespace: Namespace::Other(uri), name, .. } if uri.is_empty() && name == "g")
        );
        let nested_template = document.next_sibling(g).unwrap().unwrap();
        let nested_content = document.template_content(nested_template).unwrap().unwrap();
        assert_eq!(document.first_child(nested_template).unwrap(), None);
        assert!(document.first_child(nested_content).unwrap().is_some());
        let text = document.next_sibling(nested_template).unwrap().unwrap();
        assert!(matches!(document.kind(text).unwrap(), NodeKind::Text(value) if value == "text"));
        let comment = document.next_sibling(text).unwrap().unwrap();
        assert!(
            matches!(document.kind(comment).unwrap(), NodeKind::Comment(value) if value == "note")
        );
        let cdata = document.next_sibling(comment).unwrap().unwrap();
        assert!(matches!(document.kind(cdata).unwrap(), NodeKind::CData(value) if value == "data"));
        let sibling = document.next_sibling(template).unwrap().unwrap();
        assert!(
            matches!(document.kind(sibling).unwrap(), NodeKind::Element { namespace: Namespace::Other(uri), .. } if uri.as_ref() == "urn:default")
        );
        let clone = document.clone_subtree(template).unwrap();
        assert!(document
            .first_child(document.template_content(clone).unwrap().unwrap())
            .unwrap()
            .is_some());
        let fragment = parse_fragment_in(
            &mut document,
            root,
            "<h:template xmlns=''><g/></h:template><sibling/>",
        )
        .unwrap();
        let fragment_template = document.first_child(fragment).unwrap().unwrap();
        assert!(document
            .first_child(
                document
                    .template_content(fragment_template)
                    .unwrap()
                    .unwrap()
            )
            .unwrap()
            .is_some());
        assert_eq!(document.first_child(fragment_template).unwrap(), None);
    }

    #[test]
    fn xml_template_creation_respects_namespace_case_and_fragment_node_budget() {
        for (markup, is_template) in [
            ("<h:template xmlns:h='http://www.w3.org/1999/xhtml'/>", true),
            ("<template xmlns='http://www.w3.org/1999/xhtml'/>", true),
            (
                "<h:Template xmlns:h='http://www.w3.org/1999/xhtml'/>",
                false,
            ),
            ("<h:template xmlns:h='urn:foreign'/>", false),
        ] {
            let document = parse(markup, 3).unwrap();
            let root = root_element(&document);
            assert_eq!(
                document.template_content(root).unwrap().is_some(),
                is_template
            );
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
    fn xml_parser_preserves_trailing_processing_instruction_data() {
        let document = parse("<?ready yes ?><root/>", 8).unwrap();
        let instruction = document.first_child(document.root()).unwrap().unwrap();
        assert!(matches!(
            document.kind(instruction).unwrap(),
            NodeKind::ProcessingInstruction { target, data }
                if target == "ready" && data == "yes "
        ));
        assert_eq!(outer_html(&document, instruction).unwrap(), "<?ready yes ?>");
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
    fn xml_literal_colon_name_metadata_survives_copy_move_and_reclamation() {
        let mut source = Document::new(16);
        let mut target = Document::new(16);
        let literal = source.create_unprefixed_element(Namespace::Html, "p:name".into(), Vec::new()).unwrap();
        assert_eq!(source.element_name_parts(literal).unwrap(), (None, "p:name"));
        assert_eq!(outer_html(&source, literal), Err(DomError::WrongKind));
        assert_eq!(serialize_xml(&source, literal, false).unwrap(),
            "<p:name xmlns=\"http://www.w3.org/1999/xhtml\"></p:name>");
        let clone = source.clone_node(literal, true).unwrap();
        let imported = target.clone_subtree_from(&source, literal, true).unwrap();
        assert_eq!(source.element_name_parts(clone).unwrap(), (None, "p:name"));
        assert_eq!(target.element_name_parts(imported).unwrap(), (None, "p:name"));
        assert!(crate::equality::is_equal_node(&source, literal, &target, imported).unwrap());
        let mut full = Document::new(1);
        assert!(matches!(full.adopt_subtree_from(&mut source, literal), Err(DomError::LimitExceeded)));
        assert_eq!(source.element_name_parts(literal).unwrap(), (None, "p:name"));
        assert_eq!(source.literal_colon_names.len(), 2);
        assert!(full.literal_colon_names.is_empty());
        let (adopted, _) = target.adopt_subtree_from(&mut source, literal).unwrap();
        assert_eq!(target.element_name_parts(adopted).unwrap(), (None, "p:name"));
        assert_eq!(source.literal_colon_names, alloc::vec![clone]);
        source.destroy_subtree(clone).unwrap();
        assert_eq!(source.literal_colon_names.capacity(), 0);
        target.destroy_subtree(imported).unwrap();
        target.destroy_subtree(adopted).unwrap();
        assert_eq!(target.literal_colon_names.capacity(), 0);
        for _ in 0..100 {
            let literal = target.create_unprefixed_element(Namespace::Html, "p:name".into(), Vec::new()).unwrap();
            target.destroy_subtree(literal).unwrap();
            let prefixed = target.create(NodeKind::Element { namespace: Namespace::Html, name: "p:name".into(), attributes: Vec::new() }).unwrap();
            assert_eq!(target.element_name_parts(prefixed).unwrap(), (Some("p"), "name"));
            assert!(outer_html(&target, prefixed).unwrap().contains("xmlns:p="));
            assert!(target.literal_colon_names.is_empty());
            target.destroy_subtree(prefixed).unwrap();
            assert_eq!(target.node_count(), 1);
        }
        let ordinary = target.create_unprefixed_element(Namespace::Html, "div".into(), Vec::new()).unwrap();
        assert_eq!(target.element_name_parts(ordinary).unwrap(), (None, "div"));
        assert_eq!(target.literal_colon_names.capacity(), 0);
    }

    #[test]
    fn xml_inner_html_reuses_scoped_serializer_without_retained_arena_nodes() {
        let mut document = parse("<root xmlns:p='urn:p'><p:child/><p:child/></root>", 16).unwrap();
        let root = root_element(&document);
        let before = document.node_count();
        let expected = "<p:child xmlns:p=\"urn:p\"/><p:child xmlns:p=\"urn:p\"/>";
        for _ in 0..100 {
            let output = inner_html(&document, root).unwrap();
            assert_eq!(output, expected);
            let roundtrip = parse(&alloc::format!("<root>{output}</root>"), 16).unwrap();
            let roundtrip_root = root_element(&roundtrip);
            assert_eq!(inner_html(&roundtrip, roundtrip_root).unwrap(), expected);
            let child = roundtrip.first_child(roundtrip_root).unwrap().unwrap();
            assert!(matches!(roundtrip.kind(child).unwrap(), NodeKind::Element { namespace: Namespace::Other(uri), name, .. }
                if uri.as_ref() == "urn:p" && name == "p:child"));
            assert_eq!(document.node_count(), before);
        }
        let child = document.first_child(root).unwrap().unwrap();
        let invalid = document.create(NodeKind::Text(String::from("\u{c}"))).unwrap();
        document.append(child, invalid).unwrap();
        let before = document.node_count();
        for _ in 0..100 {
            assert_eq!(inner_html(&document, root), Err(DomError::WrongKind));
            assert!(serialize_xml(&document, root, false).unwrap().contains('\u{c}'));
            assert_eq!(document.node_count(), before);
        }
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
    fn xml_serializer_uses_dom_name_components_in_lenient_mode() {
        let mut document = parse("<root/>", 16).unwrap();
        let root = root_element(&document);
        document
            .set_attribute(root, "literal:attribute", "one")
            .unwrap();
        document
            .set_attribute_ns(root, Some("urn:attributes"), "9:local:part", "two")
            .unwrap();

        let numeric_prefix = document
            .create(NodeKind::Element {
                namespace: Namespace::Other(Rc::from("urn:numeric-prefix")),
                name: Name::new("1:item"),
                attributes: Vec::new(),
            })
            .unwrap();
        let multiple_colons = document
            .create(NodeKind::Element {
                namespace: Namespace::Other(Rc::from("urn:multiple-colons")),
                name: Name::new("p:local:part"),
                attributes: Vec::new(),
            })
            .unwrap();
        let literal_colon = document
            .create_unprefixed_element(
                Namespace::Other(Rc::from("")),
                Name::new("literal:element"),
                Vec::new(),
            )
            .unwrap();
        document.append(root, numeric_prefix).unwrap();
        document.append(root, multiple_colons).unwrap();
        document.append(root, literal_colon).unwrap();

        assert_eq!(
            serialize_xml(&document, root, false).unwrap(),
            "<root literal:attribute=\"one\" xmlns:9=\"urn:attributes\" 9:local:part=\"two\"><1:item xmlns:1=\"urn:numeric-prefix\"/><p:local:part xmlns:p=\"urn:multiple-colons\"/><literal:element/></root>"
        );
        assert_eq!(
            serialize_xml(&document, root, true),
            Err(DomError::WrongKind)
        );

        let mut invalid_attribute = parse("<root/>", 8).unwrap();
        let root = root_element(&invalid_attribute);
        invalid_attribute
            .set_attribute_ns(root, Some("urn:attributes"), "p:local:part", "value")
            .unwrap();
        assert_eq!(
            serialize_xml(&invalid_attribute, root, false).unwrap(),
            "<root xmlns:p=\"urn:attributes\" p:local:part=\"value\"/>"
        );
        assert_eq!(
            serialize_xml(&invalid_attribute, root, true),
            Err(DomError::WrongKind)
        );

        let mut lookalike_declaration = parse("<root/>", 8).unwrap();
        let root = root_element(&lookalike_declaration);
        lookalike_declaration
            .set_attribute(root, "xmlns:p", "urn:lookalike")
            .unwrap();
        let child = lookalike_declaration
            .create(NodeKind::Element {
                namespace: Namespace::Other(Rc::from("urn:lookalike")),
                name: Name::new("p:child"),
                attributes: Vec::new(),
            })
            .unwrap();
        lookalike_declaration.append(root, child).unwrap();
        assert_eq!(
            serialize_xml(&lookalike_declaration, root, false).unwrap(),
            "<root xmlns:p=\"urn:lookalike\"><p:child xmlns:p=\"urn:lookalike\"/></root>"
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
    fn xml_serializer_repairs_authored_default_declarations_and_rebound_attribute_prefixes() {
        let mut document = parse("<package><manifest/></package>", 8).unwrap();
        let root = root_element(&document);
        let child = document.first_child(root).unwrap().unwrap();
        for node in [root, child] {
            document
                .set_attribute_ns(
                    node,
                    Some(XMLNS_NAMESPACE),
                    "xmlns",
                    "http://www.idpf.org/2007/opf",
                )
                .unwrap();
        }
        assert_eq!(serialize_xml(&document, root, false).unwrap(), "<package><manifest/></package>");

        let mut document = parse("<root xmlns:p='uri1'><child/></root>", 8).unwrap();
        let root = root_element(&document);
        let child = document.first_child(root).unwrap().unwrap();
        document.set_attribute_ns(child, Some("uri2"), "p:foobar", "v").unwrap();
        assert_eq!(serialize_xml(&document, root, false).unwrap(), "<root xmlns:p=\"uri1\"><child xmlns:p=\"uri2\" p:foobar=\"v\"/></root>");

        for (source, expected) in [
            (
                "<root><child xmlns=\"\"/></root>",
                "<root><child/></root>",
            ),
            (
                "<root xmlns=\"\"><child xmlns=\"\"/></root>",
                "<root><child/></root>",
            ),
            (
                "<root xmlns=\"u1\"><child xmlns=\"u1\"/></root>",
                "<root xmlns=\"u1\"><child/></root>",
            ),
        ] {
            let document = parse(source, 8).unwrap();
            let root = root_element(&document);
            assert_eq!(serialize_xml(&document, root, false).unwrap(), expected);
        }

        let mut document = parse("<root xmlns='' xmlns:foo='urn:bar'/>", 8).unwrap();
        let root = root_element(&document);
        document.set_attribute_ns(root, Some(XMLNS_NAMESPACE), "xmlns:foo", "").unwrap();
        assert_eq!(serialize_xml(&document, root, false).unwrap(), "<root xmlns:foo=\"\"/>");
        assert_eq!(serialize_xml(&document, root, true), Err(DomError::WrongKind));
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
            &NodeKind::Text("tail<i/>".into())
        );
        // A character reference in an entity value is expanded when the entity is declared, so
        // the replacement text `<i/>` is markup and `&lt;` is a reference to the less-than sign.
        let numeric = document.next_sibling(trailing).unwrap().unwrap();
        assert!(matches!(
            document.kind(numeric),
            Ok(NodeKind::Element { name, .. }) if name == "i"
        ));
        let last = document.next_sibling(numeric).unwrap().unwrap();
        assert_eq!(document.kind(last).unwrap(), &NodeKind::Text("<Z".into()));

        // Entity declarations are parser input only; the DOM doctype writer
        // serializes the identifiers and name without copying the subset.
        assert_eq!(
            serialize_xml(&document, document.root(), false).unwrap(),
            "<!DOCTYPE foo><foo title=\"hello world\">Ahello world:<b title=\"hello world\">hello world</b>tail&lt;i/&gt;<i/>&lt;Z</foo>"
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
    fn dom_factory_names_follow_context_sensitive_name_validation() {
        assert!(is_valid_element_local_name("section"));
        assert!(is_valid_element_local_name("a=b"));
        assert!(is_valid_element_local_name("a:b:c"));
        assert!(is_valid_element_local_name(":_é"));
        assert!(!is_valid_element_local_name(""));
        assert!(!is_valid_element_local_name("9name"));
        assert!(!is_valid_element_local_name("a/b"));
        assert!(!is_valid_element_local_name("a b"));

        assert!(is_valid_doctype_name(""));
        assert!(is_valid_doctype_name("html:5"));
        assert!(!is_valid_doctype_name("html name"));
        assert!(!is_valid_doctype_name("html>"));
        assert!(!is_valid_doctype_name("html\0"));

        assert_eq!(
            validate_dom_qualified_name(Some(""), "p:a:b", DomNameContext::Element),
            Err(DomNameError::Namespace)
        );
        let parsed = validate_dom_qualified_name(
            Some("urn:x"),
            "p:a:b",
            DomNameContext::Element,
        )
        .unwrap();
        assert_eq!(parsed.namespace, Some("urn:x"));
        assert_eq!(parsed.prefix, Some("p"));
        assert_eq!(parsed.local_name, "a:b");

        assert_eq!(
            validate_dom_qualified_name(None, "p:name", DomNameContext::Element),
            Err(DomNameError::Namespace)
        );
        assert_eq!(
            validate_dom_qualified_name(Some(XML_NAMESPACE), "xml:name", DomNameContext::Attribute)
                .unwrap()
                .local_name,
            "name"
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "p:a=b", DomNameContext::Element)
                .unwrap()
                .local_name,
            "a=b"
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "p:a=b", DomNameContext::Attribute),
            Err(DomNameError::InvalidCharacter)
        );
        assert_eq!(
            validate_dom_qualified_name(Some(XMLNS_NAMESPACE), "xmlns:decl", DomNameContext::Attribute)
                .unwrap()
                .prefix,
            Some("xmlns")
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "xmlns:decl", DomNameContext::Attribute),
            Err(DomNameError::Namespace)
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "p:bad/name", DomNameContext::Element),
            Err(DomNameError::InvalidCharacter)
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "a=b:name", DomNameContext::Element)
                .unwrap()
                .prefix,
            Some("a=b")
        );
        assert_eq!(
            validate_dom_qualified_name(Some("urn:x"), "bad/p:name", DomNameContext::Element),
            Err(DomNameError::InvalidCharacter)
        );
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
