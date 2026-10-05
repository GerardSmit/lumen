//! Bounded, non-validating XML parsing into the shared document arena.
//!
//! Recognized XHTML/MathML public identifiers use the shared static character
//! entity catalog. Bounded internal general entities in the internal subset
//! are expanded locally; external subsets/entities are never loaded.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};
use core::cell::Cell;

const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 512;
const MAX_ENTITY_DECLARATIONS: usize = 256;
const MAX_ENTITY_NAME_BYTES: usize = 1024;
const MAX_ENTITY_VALUE_BYTES: usize = 64 * 1024;
const MAX_ENTITY_VALUES_BYTES: usize = 256 * 1024;
const MAX_ENTITY_EXPANSION_BYTES: usize = 1024 * 1024;
const MAX_ENTITY_EXPANSIONS: usize = 4096;
const MAX_ENTITY_DEPTH: usize = 32;

#[derive(Clone)]
struct GeneralEntity {
    name: String,
    // None denotes an external entity. This parser recognizes the declaration
    // but deliberately has no resolver or transport path for it.
    replacement: Option<Rc<str>>,
}

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
    if input.len() > MAX_XML_BYTES {
        return Err(error(0, "XML input too large"));
    }
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    if let Some((offset, _)) = input.char_indices().find(|(_, ch)| !xml_char(*ch)) {
        return Err(error(offset, "invalid XML character"));
    }
    let mut document = Document::new(max_nodes);
    initialize(&mut document);
    let mut parser = Parser {
        input,
        pos: 0,
        document,
        roots: 0,
        depth: 0,
        doctype_name: None,
        html_entity_catalog: false,
        root_name: None,
        entities: Rc::new(Vec::new()),
        entity_budget: Rc::new(Cell::new(MAX_ENTITY_EXPANSION_BYTES)),
        entity_expansions: Rc::new(Cell::new(MAX_ENTITY_EXPANSIONS)),
        entity_stack: Vec::new(),
    };
    let root = parser.document.root();
    if parser.starts("<?xml")
        && parser
            .input
            .as_bytes()
            .get(parser.pos + 5)
            .is_some_and(u8::is_ascii_whitespace)
    {
        parser.processing_instruction(true)?;
    }
    parser.skip_space();
    loop {
        parser.skip_space();
        if parser.eof() {
            break;
        }
        if parser.starts("<!--") {
            parser.comment(root)?;
        } else if parser.starts("<?") {
            parser.processing_instruction(false)?;
        } else if parser.starts("<!DOCTYPE") {
            if parser.roots != 0 || parser.doctype_name.is_some() {
                return Err(error(parser.pos, "duplicate or misplaced doctype"));
            }
            parser.doctype(root)?;
        } else if parser.starts("<") {
            if parser.roots != 0 {
                return Err(error(parser.pos, "multiple document elements"));
            }
            let initial = vec![
                (
                    String::from("xml"),
                    String::from("http://www.w3.org/XML/1998/namespace"),
                ),
                (
                    String::from("xmlns"),
                    String::from("http://www.w3.org/2000/xmlns/"),
                ),
            ];
            parser.element(root, initial)?;
            parser.roots = 1;
        } else {
            let start = parser.pos;
            while !parser.eof() && !parser.starts("<") {
                parser.bump_char();
            }
            if !parser.input[start..parser.pos]
                .chars()
                .all(char::is_whitespace)
            {
                return Err(error(start, "character data outside document element"));
            }
        }
    }
    if parser.roots != 1 {
        return Err(error(parser.pos, "document has no document element"));
    }
    if parser
        .doctype_name
        .as_deref()
        .is_some_and(|name| parser.root_name.as_deref() != Some(name))
    {
        return Err(error(
            parser.pos,
            "doctype name does not match document element",
        ));
    }
    Ok(parser.document)
}

/// Parse a well-formed XML fragment using the namespace bindings in an element
/// context and move the completed fragment into the caller's document.
///
/// Parsing happens in a bounded temporary arena, so malformed input never
/// leaves partial nodes in the destination tree. The temporary arena is sized
/// from the destination's remaining node budget; adoption performs the same
/// capacity check before it mutates either document.
pub fn parse_fragment_in(
    document: &mut Document,
    context: NodeId,
    input: &str,
) -> Result<NodeId, ParseError> {
    if input.len() > MAX_XML_BYTES {
        return Err(error(0, "XML input too large"));
    }
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    if let Some((offset, _)) = input.char_indices().find(|(_, ch)| !xml_char(*ch)) {
        return Err(error(offset, "invalid XML character"));
    }
    let (namespaces, html_entity_catalog) = fragment_context(document, context)?;
    let remaining = document.max_nodes.saturating_sub(document.live_nodes);
    let mut parser = Parser {
        input,
        pos: 0,
        document: Document::new(remaining.saturating_add(1)),
        roots: 0,
        depth: 0,
        doctype_name: None,
        html_entity_catalog,
        root_name: None,
        entities: Rc::new(Vec::new()),
        entity_budget: Rc::new(Cell::new(MAX_ENTITY_EXPANSION_BYTES)),
        entity_expansions: Rc::new(Cell::new(MAX_ENTITY_EXPANSIONS)),
        entity_stack: Vec::new(),
    };
    let fragment = parser.create(NodeKind::DocumentFragment)?;
    if let Err(parse_error) = parser.fragment_contents(fragment, namespaces) {
        let _ = parser.document.destroy_subtree(fragment);
        return Err(parse_error);
    }
    document
        .adopt_subtree_from(&mut parser.document, fragment)
        .map(|(fragment, _)| fragment)
        .map_err(|error_kind| match error_kind {
            DomError::LimitExceeded => error(parser.pos, "XML node limit exceeded"),
            _ => error(parser.pos, "invalid XML fragment placement"),
        })
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
                bind_namespace(&mut namespaces, prefix, value, 0)?;
            }
            if id == context {
                let (prefix, _) = split_qname(name.as_str())
                    .ok_or_else(|| error(0, "invalid context qualified name"))?;
                if namespace_for(&namespaces, prefix).is_none() {
                    let uri = namespace_uri(namespace);
                    bind_namespace(&mut namespaces, prefix, uri, 0)?;
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

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    document: Document,
    roots: usize,
    depth: usize,
    doctype_name: Option<String>,
    html_entity_catalog: bool,
    root_name: Option<String>,
    entities: Rc<Vec<GeneralEntity>>,
    entity_budget: Rc<Cell<usize>>,
    entity_expansions: Rc<Cell<usize>>,
    entity_stack: Vec<usize>,
}

impl Parser<'_> {
    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }
    fn starts(&self, text: &str) -> bool {
        self.input[self.pos..].starts_with(text)
    }
    fn bump_char(&mut self) {
        if let Some(ch) = self.input[self.pos..].chars().next() {
            self.pos += ch.len_utf8();
        }
    }
    fn skip_space(&mut self) {
        while !self.eof() && self.input.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }
    fn consume(&mut self, text: &str) -> Result<(), ParseError> {
        if self.starts(text) {
            self.pos += text.len();
            Ok(())
        } else {
            Err(error(self.pos, "unexpected token"))
        }
    }
    fn name(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        let mut chars = self.input[self.pos..].char_indices();
        let Some((_, first)) = chars.next() else {
            return Err(error(self.pos, "expected XML name"));
        };
        if !name_start(first) {
            return Err(error(self.pos, "invalid XML name"));
        }
        self.pos += first.len_utf8();
        while !self.eof() {
            let ch = self.input[self.pos..].chars().next().unwrap();
            if !name_char(ch) {
                break;
            }
            self.pos += ch.len_utf8();
        }
        Ok(String::from(&self.input[start..self.pos]))
    }
    fn comment(&mut self, parent: NodeId) -> Result<(), ParseError> {
        self.consume("<!--")?;
        let start = self.pos;
        let Some(end) = self.input[self.pos..].find("-->") else {
            return Err(error(self.pos, "unterminated comment"));
        };
        let text = &self.input[start..start + end];
        if text.contains("--") || text.ends_with('-') {
            return Err(error(start, "invalid comment"));
        }
        self.pos = start + end + 3;
        let id = self.create(NodeKind::Comment(String::from(text)))?;
        self.document
            .append(parent, id)
            .map_err(|_| error(start, "invalid comment placement"))
    }
    fn processing_instruction(&mut self, declaration: bool) -> Result<(), ParseError> {
        self.consume("<?")?;
        let start = self.pos;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") && !declaration {
            return Err(error(start, "reserved processing instruction target"));
        }
        let data_start = self.pos;
        let Some(end) = self.input[self.pos..].find("?>") else {
            return Err(error(self.pos, "unterminated processing instruction"));
        };
        let raw_data = &self.input[data_start..data_start + end];
        if !raw_data.is_empty() && !raw_data.starts_with(char::is_whitespace) {
            return Err(error(
                data_start,
                "expected whitespace after processing instruction target",
            ));
        }
        if declaration {
            validate_xml_declaration(raw_data, data_start)?;
        }
        let data = raw_data.trim();
        self.pos = data_start + end + 2;
        if !declaration {
            let id = self.create(NodeKind::ProcessingInstruction {
                target,
                data: String::from(data),
            })?;
            self.document
                .append(self.document.root(), id)
                .map_err(|_| error(start, "invalid processing instruction"))?;
        }
        Ok(())
    }
    fn doctype(&mut self, parent: NodeId) -> Result<(), ParseError> {
        let start = self.pos;
        self.consume("<!DOCTYPE")?;
        if !self.input[self.pos..].starts_with(char::is_whitespace) {
            return Err(error(self.pos, "expected doctype name"));
        }
        self.skip_space();
        let name = self.name()?;
        self.doctype_name = Some(name.clone());
        self.skip_space();
        let mut public_id = String::new();
        let mut system_id = String::new();
        if self.starts("PUBLIC") {
            self.consume("PUBLIC")?;
            self.external_id_space()?;
            public_id = self.external_literal()?.to_string();
            if !public_id.bytes().all(public_id_char) {
                return Err(error(start, "invalid public identifier"));
            }
            let catalog = known_character_entity_catalog(&public_id);
            self.external_id_space()?;
            system_id = self.external_literal()?.to_string();
            self.html_entity_catalog = catalog;
            self.skip_space();
        } else if self.starts("SYSTEM") {
            self.consume("SYSTEM")?;
            self.external_id_space()?;
            system_id = self.external_literal()?.to_string();
            self.skip_space();
        }
        if !self.starts("[") && !self.starts(">") {
            return Err(error(self.pos, "invalid doctype external identifier"));
        }
        if self.starts("[") {
            self.pos += 1;
            self.internal_subset()?;
            self.skip_space();
        }
        self.consume(">")?;
        let id = self.create(NodeKind::DocumentType(name))?;
        self.document
            .append(parent, id)
            .map_err(|_| error(start, "invalid doctype placement"))?;
        self.document
            .set_doctype_identifiers(id, &public_id, &system_id)
            .map_err(|_| error(start, "invalid doctype identifiers"))?;
        Ok(())
    }

    fn internal_subset(&mut self) -> Result<(), ParseError> {
        let mut entities: Vec<GeneralEntity> = Vec::new();
        let mut entity_value_bytes = 0usize;
        let mut declaration_count = 0usize;
        loop {
            self.skip_space();
            if self.eof() {
                return Err(error(self.pos, "unterminated doctype internal subset"));
            }
            if self.starts("]") {
                self.pos += 1;
                entities.sort_unstable_by(|left, right| left.name.cmp(&right.name));
                self.entities = Rc::new(entities);
                return Ok(());
            }
            if self.starts("<!--") {
                let start = self.pos;
                self.pos += 4;
                let Some(end) = self.input[self.pos..].find("-->") else {
                    return Err(error(start, "unterminated DTD comment"));
                };
                let comment = &self.input[self.pos..self.pos + end];
                if comment.contains("--") || comment.ends_with('-') {
                    return Err(error(start, "invalid DTD comment"));
                }
                self.pos += end + 3;
                continue;
            }
            if self.starts("<?") {
                self.pos += 2;
                let Some(end) = self.input[self.pos..].find("?>") else {
                    return Err(error(self.pos, "unterminated DTD processing instruction"));
                };
                self.pos += end + 2;
                continue;
            }
            if self.starts("<!ENTITY") {
                let after_keyword = self.pos + "<!ENTITY".len();
                if self
                    .input
                    .as_bytes()
                    .get(after_keyword)
                    .is_some_and(u8::is_ascii_whitespace)
                {
                    declaration_count = declaration_count.saturating_add(1);
                    if declaration_count > MAX_ENTITY_DECLARATIONS {
                        return Err(error(self.pos, "too many DTD entity declarations"));
                    }
                    self.entity_declaration(&mut entities, &mut entity_value_bytes)?;
                    continue;
                }
                return Err(error(self.pos, "expected whitespace after ENTITY"));
            }
            if self.starts("<!") {
                self.skip_dtd_declaration()?;
                continue;
            }
            if self.starts("%") {
                let start = self.pos;
                let Some(end) = self.input[self.pos..].find(';') else {
                    return Err(error(start, "unterminated DTD parameter reference"));
                };
                self.pos += end + 1;
                return Err(error(
                    start,
                    "DTD parameter-entity references are unsupported",
                ));
            }
            return Err(error(self.pos, "invalid DTD internal subset"));
        }
    }

    fn entity_declaration(
        &mut self,
        entities: &mut Vec<GeneralEntity>,
        entity_value_bytes: &mut usize,
    ) -> Result<(), ParseError> {
        let start = self.pos;
        self.consume("<!ENTITY")?;
        let before_space = self.pos;
        self.skip_space();
        if self.pos == before_space {
            return Err(error(self.pos, "expected whitespace after ENTITY"));
        }
        if self.starts("%") {
            self.pos = start;
            self.skip_dtd_declaration()?;
            return Ok(());
        }
        let name = self.name()?;
        if name.len() > MAX_ENTITY_NAME_BYTES {
            return Err(error(start, "DTD entity name too large"));
        }
        let before_space = self.pos;
        self.skip_space();
        if self.pos == before_space {
            return Err(error(self.pos, "expected entity definition"));
        }

        let replacement = if self.starts("'") || self.starts("\"") {
            let value = self.entity_value()?;
            *entity_value_bytes = entity_value_bytes
                .checked_add(value.len())
                .filter(|total| *total <= MAX_ENTITY_VALUES_BYTES)
                .ok_or_else(|| error(start, "DTD entity values too large"))?;
            if value.len() > MAX_ENTITY_VALUE_BYTES {
                return Err(error(start, "DTD entity value too large"));
            }
            Some(Rc::<str>::from(value))
        } else {
            self.external_entity_definition()?;
            None
        };
        self.skip_space();
        self.consume(">")?;

        // XML's first declaration for a general entity is binding. Keep the
        // internal-subset declaration separate from the optional XHTML/MathML
        // catalog so it naturally overrides that external catalog.
        if !entities.iter().any(|entity| entity.name == name) {
            entities.push(GeneralEntity { name, replacement });
        }
        Ok(())
    }

    fn entity_value(&mut self) -> Result<String, ParseError> {
        let quote = self.input.as_bytes()[self.pos] as char;
        self.pos += 1;
        let start = self.pos;
        while !self.eof() && self.input.as_bytes()[self.pos] != quote as u8 {
            self.bump_char();
        }
        if self.eof() {
            return Err(error(start, "unterminated DTD entity value"));
        }
        let value = &self.input[start..self.pos];
        if value.contains('%') {
            return Err(error(
                start,
                "parameter-entity references in EntityValue are unsupported",
            ));
        }
        validate_entity_value_references(value, start)?;
        self.pos += quote.len_utf8();
        if value.len() > MAX_ENTITY_VALUE_BYTES {
            return Err(error(start, "DTD entity value too large"));
        }
        Ok(String::from(value))
    }

    fn external_entity_definition(&mut self) -> Result<(), ParseError> {
        if self.starts("SYSTEM") {
            self.consume("SYSTEM")?;
            self.external_id_space()?;
            let _ = self.external_literal()?;
        } else if self.starts("PUBLIC") {
            self.consume("PUBLIC")?;
            self.external_id_space()?;
            let _ = self.external_literal()?;
            self.external_id_space()?;
            let _ = self.external_literal()?;
        } else {
            return Err(error(self.pos, "invalid DTD entity definition"));
        }
        let before_space = self.pos;
        self.skip_space();
        if self.starts("NDATA") {
            if self.pos == before_space {
                return Err(error(self.pos, "expected whitespace before NDATA"));
            }
            self.consume("NDATA")?;
            self.external_id_space()?;
            let _ = self.name()?;
        }
        Ok(())
    }

    fn skip_dtd_declaration(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.consume("<")?;
        let mut quote = None;
        while !self.eof() {
            let ch = self.input[self.pos..].chars().next().unwrap();
            self.pos += ch.len_utf8();
            if let Some(current_quote) = quote {
                if ch == current_quote {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '>' => return Ok(()),
                _ => {}
            }
        }
        Err(error(start, "unterminated DTD declaration"))
    }

    fn fragment_contents(
        &mut self,
        parent: NodeId,
        inherited: Vec<(String, String)>,
    ) -> Result<(), ParseError> {
        while !self.eof() {
            if self.starts("<!--") {
                self.comment(parent)?;
            } else if self.starts("<?") {
                self.processing_instruction_for(parent)?;
            } else if self.starts("<![CDATA[") {
                self.cdata(parent)?;
            } else if self.starts("<!") {
                return Err(error(
                    self.pos,
                    "unsupported markup declaration in XML fragment",
                ));
            } else if self.starts("<") {
                self.element(parent, inherited.clone())?;
            } else {
                self.character_data_or_entity(parent)?;
            }
        }
        Ok(())
    }

    fn cdata(&mut self, parent: NodeId) -> Result<(), ParseError> {
        self.consume("<![CDATA[")?;
        let data_at = self.pos;
        let Some(end) = self.input[self.pos..].find("]]>") else {
            return Err(error(data_at, "unterminated CDATA section"));
        };
        let text = String::from(&self.input[data_at..data_at + end]);
        self.pos = data_at + end + 3;
        let cdata = self.create(NodeKind::CData(text))?;
        self.document
            .append(parent, cdata)
            .map_err(|_| error(data_at, "invalid CDATA placement"))
    }

    fn external_id_space(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.skip_space();
        if self.pos == start {
            return Err(error(start, "expected external identifier whitespace"));
        }
        Ok(())
    }
    fn external_literal(&mut self) -> Result<&str, ParseError> {
        let quote = *self
            .input
            .as_bytes()
            .get(self.pos)
            .filter(|&&quote| matches!(quote, b'\'' | b'"'))
            .ok_or_else(|| error(self.pos, "expected quoted external identifier"))?;
        let start = self.pos + 1;
        let end = self.input[start..]
            .find(quote as char)
            .ok_or_else(|| error(start, "unterminated external identifier"))?
            + start;
        self.pos = end + 1;
        Ok(&self.input[start..end])
    }
    fn element(
        &mut self,
        parent: NodeId,
        inherited: Vec<(String, String)>,
    ) -> Result<(), ParseError> {
        if self.depth >= MAX_DEPTH {
            return Err(error(self.pos, "XML nesting limit exceeded"));
        }
        let start = self.pos;
        self.consume("<")?;
        let qname = self.name()?;
        if parent == self.document.root() {
            self.root_name = Some(qname.clone());
        }
        let mut attributes: Vec<(String, String)> = Vec::new();
        let mut self_closing = false;
        let mut require_space = false;
        loop {
            let before_space = self.pos;
            self.skip_space();
            let had_space = self.pos != before_space;
            if self.starts("/>") {
                self.pos += 2;
                self_closing = true;
                break;
            }
            if self.starts(">") {
                self.pos += 1;
                break;
            }
            if self.eof() {
                return Err(error(start, "unterminated start tag"));
            }
            if require_space && !had_space {
                return Err(error(self.pos, "expected whitespace before attribute"));
            }
            let attr_at = self.pos;
            let name = self.name()?;
            if attributes.iter().any(|(seen, _)| seen == &name) {
                return Err(error(attr_at, "duplicate XML attribute"));
            }
            self.skip_space();
            self.consume("=")?;
            self.skip_space();
            let value = self.quoted_value()?;
            attributes.push((name, value));
            require_space = true;
        }
        let mut namespaces = inherited;
        for (name, value) in &attributes {
            if name == "xmlns" {
                bind_namespace(&mut namespaces, "", value, start)?;
            } else if let Some(prefix) = name.strip_prefix("xmlns:") {
                if prefix.is_empty() || split_qname(prefix).is_none_or(|(p, _)| !p.is_empty()) {
                    return Err(error(start, "invalid namespace prefix"));
                }
                bind_namespace(&mut namespaces, prefix, value, start)?;
            }
        }
        let (prefix, local) =
            split_qname(&qname).ok_or_else(|| error(start, "invalid qualified name"))?;
        let uri = if prefix.is_empty() {
            namespace_for(&namespaces, "")
        } else {
            Some(
                namespace_for(&namespaces, prefix)
                    .ok_or_else(|| error(start, "unbound element prefix"))?,
            )
        };
        let mut expanded_names: Vec<(Option<String>, String)> = Vec::new();
        let mut namespace_metadata: Vec<(String, Option<String>)> = Vec::new();
        for (name, _) in &attributes {
            if name == "xmlns" || name.starts_with("xmlns:") {
                namespace_metadata.push((
                    name.clone(),
                    Some(String::from("http://www.w3.org/2000/xmlns/")),
                ));
                continue;
            }
            let (prefix, local) = split_qname(name)
                .ok_or_else(|| error(start, "invalid attribute qualified name"))?;
            let uri = if prefix.is_empty() {
                None
            } else {
                Some(
                    namespace_for(&namespaces, prefix)
                        .ok_or_else(|| error(start, "unbound attribute prefix"))?,
                )
            };
            if expanded_names
                .iter()
                .any(|(seen_uri, seen_local)| seen_uri == &uri && seen_local == local)
            {
                return Err(error(start, "duplicate expanded attribute name"));
            }
            expanded_names.push((uri, String::from(local)));
            namespace_metadata.push((
                name.clone(),
                if prefix.is_empty() {
                    None
                } else {
                    namespace_for(&namespaces, prefix)
                },
            ));
        }
        let namespace = namespace_from_uri(uri.as_deref());
        let attrs = attributes
            .into_iter()
            .map(|(name, value)| (Name::new(&name), value))
            .collect();
        let id = self.create(NodeKind::Element {
            namespace,
            name: Name::new(&qname),
            attributes: attrs,
        })?;
        for (qualified_name, uri) in namespace_metadata {
            self.document
                .set_attribute_namespace_metadata(id, &qualified_name, uri.as_deref())
                .map_err(|_| error(start, "invalid attribute namespace metadata"))?;
        }
        self.document
            .append(parent, id)
            .map_err(|_| error(start, "invalid element placement"))?;
        if self_closing {
            return Ok(());
        }
        // HTML templates retain their contents in a detached fragment even
        // when their qualified name carries an XML namespace prefix.
        let child_parent = self.document.template_content(id)
            .map_err(|_| error(start, "invalid template contents"))?
            .unwrap_or(id);
        self.depth += 1;
        loop {
            if self.eof() {
                return Err(error(start, "unclosed element"));
            }
            if self.starts("</") {
                self.pos += 2;
                let close_at = self.pos;
                let close = self.name()?;
                self.skip_space();
                self.consume(">")?;
                if close != qname {
                    return Err(error(close_at, "mismatched end tag"));
                }
                self.depth -= 1;
                return Ok(());
            }
            if self.starts("<!--") {
                self.comment(child_parent)?;
            } else if self.starts("<?") {
                self.processing_instruction_for(child_parent)?;
            } else if self.starts("<![CDATA[") {
                self.cdata(child_parent)?;
            } else if self.starts("<!") {
                return Err(error(self.pos, "unsupported markup declaration"));
            } else if self.starts("<") {
                self.element(child_parent, namespaces.clone())?;
            } else {
                self.character_data_or_entity(child_parent)?;
            }
        }
    }

    fn character_data_or_entity(&mut self, parent: NodeId) -> Result<(), ParseError> {
        let mut text = String::new();
        while !self.eof() && !self.starts("<") {
            if !self.starts("&") {
                let start = self.pos;
                while !self.eof() && !self.starts("<") && !self.starts("&") {
                    self.bump_char();
                }
                let raw = &self.input[start..self.pos];
                if raw.contains("]]>") {
                    return Err(error(start, "forbidden ]]> in character data"));
                }
                append_entity_text(&mut text, raw, start)?;
                continue;
            }

            let start = self.pos;
            self.consume("&")?;
            let Some(end) = self.input[self.pos..].find(';') else {
                return Err(error(start, "unterminated entity reference"));
            };
            let name = &self.input[self.pos..self.pos + end];
            if name.is_empty() {
                return Err(error(start, "empty entity reference"));
            }
            if name.len() > MAX_ENTITY_NAME_BYTES {
                return Err(error(start, "XML entity name too large"));
            }
            let token_end = self.pos + end + 1;
            let token = &self.input[start..token_end];
            self.pos = token_end;

            if is_predefined_or_character_reference(&name) {
                let value = decode_entities(token, start, false)?;
                append_entity_text(&mut text, &value, start)?;
                continue;
            }
            if let Some(entity_index) = find_general_entity(&self.entities, name) {
                let is_internal = self.entities[entity_index].replacement.is_some();
                if !is_internal {
                    return Err(error(start, "external entity reference is unavailable"));
                }
                self.include_general_entity(parent, entity_index, start, &mut text)?;
                continue;
            }
            if self.html_entity_catalog {
                let value = decode_entities(token, start, true)?;
                append_entity_text(&mut text, &value, start)?;
                continue;
            }
            return Err(error(start, "unknown entity reference"));
        }
        self.text(parent, text)
    }

    fn include_general_entity(
        &mut self,
        parent: NodeId,
        entity_index: usize,
        reference_at: usize,
        text_run: &mut String,
    ) -> Result<(), ParseError> {
        if self.entity_stack.len() >= MAX_ENTITY_DEPTH {
            return Err(error(reference_at, "XML entity nesting limit exceeded"));
        }
        if self.entity_stack.contains(&entity_index) {
            return Err(error(reference_at, "recursive XML entity reference"));
        }
        let replacement = self
            .entities
            .get(entity_index)
            .and_then(|entity| entity.replacement.clone())
            .ok_or_else(|| error(reference_at, "external entity reference is unavailable"))?;
        spend_entity_expansion(&self.entity_expansions, reference_at)?;
        if replacement.is_empty() {
            return Ok(());
        }
        spend_entity_budget(&self.entity_budget, replacement.len(), reference_at)?;

        // Most internal entities are plain character data. Avoid constructing
        // a temporary DOM arena for each reference in that common case; retain
        // the full fragment parser whenever markup or references need parsing.
        if !replacement.contains('<') && !replacement.contains('&') && !replacement.contains("]]>")
        {
            append_entity_text(text_run, &replacement, reference_at)?;
            return Ok(());
        }

        let (namespaces, _) = fragment_context(&self.document, parent)
            .map_err(|parse_error| error(reference_at, parse_error.message))?;
        let remaining = self
            .document
            .max_nodes
            .saturating_sub(self.document.live_nodes);
        // The temporary document root and fragment wrapper are discarded after
        // parsing; reserve their two slots in addition to the destination's
        // remaining output-node budget.
        let fragment_capacity = remaining.saturating_add(2);
        let mut nested = Parser {
            input: &replacement,
            pos: 0,
            document: Document::new(fragment_capacity),
            roots: 0,
            depth: self.depth,
            doctype_name: None,
            html_entity_catalog: self.html_entity_catalog,
            root_name: None,
            entities: self.entities.clone(),
            entity_budget: self.entity_budget.clone(),
            entity_expansions: self.entity_expansions.clone(),
            entity_stack: self.entity_stack.clone(),
        };
        nested.entity_stack.push(entity_index);
        let fragment = nested
            .create(NodeKind::DocumentFragment)
            .map_err(|parse_error| error(reference_at, parse_error.message))?;
        nested
            .fragment_contents(fragment, namespaces)
            .map_err(|parse_error| error(reference_at, parse_error.message))?;

        let mut expanded_text = String::new();
        let mut all_text = true;
        let mut inspect = nested
            .document
            .first_child(fragment)
            .map_err(|_| error(reference_at, "invalid expanded entity"))?;
        while let Some(node) = inspect {
            if let NodeKind::Text(value) = nested
                .document
                .kind(node)
                .map_err(|_| error(reference_at, "invalid expanded entity"))?
            {
                append_entity_text(&mut expanded_text, value, reference_at)?;
            } else {
                all_text = false;
                break;
            }
            inspect = nested
                .document
                .next_sibling(node)
                .map_err(|_| error(reference_at, "invalid expanded entity"))?;
        }
        if all_text {
            append_entity_text(text_run, &expanded_text, reference_at)?;
            return Ok(());
        }
        if !text_run.is_empty() {
            self.text(parent, core::mem::take(text_run))?;
        }

        let mut child = nested
            .document
            .first_child(fragment)
            .map_err(|_| error(reference_at, "invalid expanded entity"))?;
        while let Some(node) = child {
            let next = nested
                .document
                .next_sibling(node)
                .map_err(|_| error(reference_at, "invalid expanded entity"))?;
            let (adopted, _) = self
                .document
                .adopt_subtree_from(&mut nested.document, node)
                .map_err(|kind| match kind {
                    DomError::LimitExceeded => error(reference_at, "XML node limit exceeded"),
                    _ => error(reference_at, "invalid expanded entity"),
                })?;
            self.append_expanded_node(parent, adopted, reference_at)?;
            child = next;
        }
        Ok(())
    }

    fn append_expanded_node(
        &mut self,
        parent: NodeId,
        child: NodeId,
        offset: usize,
    ) -> Result<(), ParseError> {
        let child_text = match self.document.kind(child) {
            Ok(NodeKind::Text(value)) => Some(value.clone()),
            _ => None,
        };
        let previous = self
            .document
            .last_child(parent)
            .map_err(|_| error(offset, "invalid expanded entity"))?;
        if let (Some(previous), Some(child_text)) = (previous, child_text) {
            let previous_text_len = match self
                .document
                .kind(previous)
                .map_err(|_| error(offset, "invalid expanded entity"))?
            {
                NodeKind::Text(value) => Some(value.len()),
                _ => None,
            };
            if let Some(previous_text_len) = previous_text_len {
                previous_text_len
                    .checked_add(child_text.len())
                    .filter(|length| *length <= MAX_XML_BYTES)
                    .ok_or_else(|| error(offset, "XML entity output too large"))?;
                self.document
                    .append_data(previous, &child_text)
                    .map_err(|_| error(offset, "invalid expanded text"))?;
                self.document
                    .destroy_subtree(child)
                    .map_err(|_| error(offset, "invalid expanded entity"))?;
                return Ok(());
            }
        }
        self.document
            .append(parent, child)
            .map_err(|_| error(offset, "invalid expanded entity placement"))
    }
    fn processing_instruction_for(&mut self, parent: NodeId) -> Result<(), ParseError> {
        self.consume("<?")?;
        let start = self.pos;
        let target = self.name()?;
        if target.eq_ignore_ascii_case("xml") {
            return Err(error(start, "reserved processing instruction target"));
        }
        let data_start = self.pos;
        let Some(end) = self.input[self.pos..].find("?>") else {
            return Err(error(self.pos, "unterminated processing instruction"));
        };
        if end > 0 && !self.input[data_start..].starts_with(char::is_whitespace) {
            return Err(error(
                data_start,
                "expected whitespace after processing instruction target",
            ));
        }
        let data = String::from(self.input[data_start..data_start + end].trim());
        self.pos = data_start + end + 2;
        let id = self.create(NodeKind::ProcessingInstruction { target, data })?;
        self.document
            .append(parent, id)
            .map_err(|_| error(start, "invalid processing instruction"))
    }
    fn quoted_value(&mut self) -> Result<String, ParseError> {
        let Some(q) = self.input[self.pos..].chars().next() else {
            return Err(error(self.pos, "expected quoted attribute value"));
        };
        if q != '\'' && q != '"' {
            return Err(error(self.pos, "expected quoted attribute value"));
        }
        self.pos += 1;
        let start = self.pos;
        while !self.eof() && !self.input[self.pos..].starts_with(q) {
            if self.starts("<") {
                return Err(error(self.pos, "less-than sign in attribute value"));
            }
            self.bump_char();
        }
        if self.eof() {
            return Err(error(start, "unterminated attribute value"));
        }
        let mut entity_stack = self.entity_stack.clone();
        let value = decode_attribute_entities(
            &self.input[start..self.pos],
            start,
            self.html_entity_catalog,
            &self.entities,
            &self.entity_budget,
            &self.entity_expansions,
            &mut entity_stack,
        )?;
        self.pos += q.len_utf8();
        Ok(value)
    }
    fn text(&mut self, parent: NodeId, text: String) -> Result<(), ParseError> {
        if text.is_empty() {
            return Ok(());
        }
        let previous = self
            .document
            .last_child(parent)
            .map_err(|_| error(self.pos, "invalid text parent"))?;
        let previous_text_len = if let Some(previous) = previous {
            if let NodeKind::Text(previous_text) = self
                .document
                .kind(previous)
                .map_err(|_| error(self.pos, "invalid text node"))?
            {
                Some((previous, previous_text.len()))
            } else {
                None
            }
        } else {
            None
        };
        if let Some((previous, previous_text_len)) = previous_text_len {
            previous_text_len
                .checked_add(text.len())
                .filter(|length| *length <= MAX_XML_BYTES)
                .ok_or_else(|| error(self.pos, "XML entity output too large"))?;
            self.document
                .append_data(previous, &text)
                .map_err(|_| error(self.pos, "invalid text node"))?;
            return Ok(());
        }
        let id = self.create(NodeKind::Text(text))?;
        self.document
            .append(parent, id)
            .map_err(|_| error(self.pos, "invalid text placement"))
    }
    fn create(&mut self, kind: NodeKind) -> Result<NodeId, ParseError> {
        self.document.create(kind).map_err(|e| match e {
            DomError::LimitExceeded => error(self.pos, "XML node limit exceeded"),
            _ => error(self.pos, "invalid XML node"),
        })
    }
}

fn name_start(ch: char) -> bool {
    matches!(ch as u32,
        0x3a | 0x41..=0x5a | 0x5f | 0x61..=0x7a | 0xc0..=0xd6 | 0xd8..=0xf6 |
        0xf8..=0x2ff | 0x370..=0x37d | 0x37f..=0x1fff | 0x200c..=0x200d |
        0x2070..=0x218f | 0x2c00..=0x2fef | 0x3001..=0xd7ff | 0xf900..=0xfdcf |
        0xfdf0..=0xfffd | 0x10000..=0xeffff)
}
fn name_char(ch: char) -> bool {
    name_start(ch)
        || matches!(ch as u32, 0x2d | 0x2e | 0x30..=0x39 | 0xb7 | 0x300..=0x36f | 0x203f..=0x2040)
}

/// Whether `name` matches the XML 1.0 `Name` production.
///
/// Unlike a QName, an XML Name may contain colons in any position.
pub fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(name_start) && chars.all(name_char)
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

fn is_ncname(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|ch| ch != ':' && name_start(ch))
        && chars.all(|ch| ch != ':' && name_char(ch))
}

fn xml_char(ch: char) -> bool {
    matches!(ch as u32, 0x9 | 0xa | 0xd | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}
fn validate_xml_declaration(input: &str, offset: usize) -> Result<(), ParseError> {
    let mut cursor = 0;
    let bytes = input.as_bytes();
    let whitespace = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n');
    if bytes.first().is_none_or(|b| !whitespace(*b)) {
        return Err(error(offset, "XML declaration requires whitespace"));
    }
    let mut fields: Vec<(&str, &str)> = Vec::new();
    while cursor < bytes.len() {
        while cursor < bytes.len() && whitespace(bytes[cursor]) {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let name_start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_alphabetic() {
            cursor += 1;
        }
        if name_start == cursor {
            return Err(error(offset + cursor, "invalid XML declaration"));
        }
        let name = &input[name_start..cursor];
        while cursor < bytes.len() && whitespace(bytes[cursor]) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            return Err(error(offset + cursor, "invalid XML declaration"));
        }
        cursor += 1;
        while cursor < bytes.len() && whitespace(bytes[cursor]) {
            cursor += 1;
        }
        let Some(&quote @ (b'\'' | b'"')) = bytes.get(cursor) else {
            return Err(error(offset + cursor, "invalid XML declaration value"));
        };
        cursor += 1;
        let value_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != quote {
            cursor += 1;
        }
        if cursor == bytes.len() {
            return Err(error(offset + cursor, "unterminated XML declaration value"));
        }
        fields.push((name, &input[value_start..cursor]));
        cursor += 1;
        if cursor < bytes.len() && !whitespace(bytes[cursor]) {
            return Err(error(
                offset + cursor,
                "expected whitespace in XML declaration",
            ));
        }
    }
    if fields.first() != Some(&("version", "1.0")) && fields.first() != Some(&("version", "1.1")) {
        return Err(error(
            offset,
            "XML declaration must start with version 1.0 or 1.1",
        ));
    }
    if fields.len() > 3 {
        return Err(error(offset, "too many XML declaration fields"));
    }
    let mut index = 1;
    if let Some(("encoding", encoding)) = fields.get(index).copied() {
        let mut chars = encoding.chars();
        if !chars.next().is_some_and(|ch| ch.is_ascii_alphabetic())
            || !chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            return Err(error(offset, "invalid XML encoding name"));
        }
        index += 1;
    }
    if let Some(("standalone", value)) = fields.get(index).copied() {
        if value != "yes" && value != "no" {
            return Err(error(offset, "invalid standalone declaration"));
        }
        index += 1;
    }
    if index != fields.len() {
        return Err(error(offset, "invalid XML declaration field order"));
    }
    Ok(())
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
    offset: usize,
) -> Result<(), ParseError> {
    let xml = "http://www.w3.org/XML/1998/namespace";
    let xmlns = "http://www.w3.org/2000/xmlns/";
    if prefix == "xmlns"
        || uri == xmlns
        || (prefix == "xml") != (uri == xml)
        || (!prefix.is_empty() && uri.is_empty())
    {
        return Err(error(offset, "invalid namespace declaration"));
    }
    bindings.retain(|(key, _)| key != prefix);
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

fn is_predefined_or_character_reference(name: &str) -> bool {
    name.starts_with('#') || matches!(name, "amp" | "lt" | "gt" | "apos" | "quot")
}

fn find_general_entity(entities: &[GeneralEntity], name: &str) -> Option<usize> {
    entities
        .binary_search_by(|entity| entity.name.as_str().cmp(name))
        .ok()
}

fn validate_entity_value_references(input: &str, offset: usize) -> Result<(), ParseError> {
    let mut rest = input;
    let mut at = offset;
    while let Some(index) = rest.find('&') {
        let after = &rest[index + 1..];
        let Some(end) = after.find(';') else {
            return Err(error(at + index, "unterminated DTD entity reference"));
        };
        let reference = &after[..end];
        if reference.is_empty() {
            return Err(error(at + index, "empty DTD entity reference"));
        }
        if reference.starts_with('#') {
            decode_entities(&rest[index..index + end + 2], at + index, false)?;
        } else if !is_xml_name(reference) {
            return Err(error(at + index, "invalid DTD entity reference"));
        }
        let used = index + end + 2;
        at += used;
        rest = &rest[used..];
    }
    Ok(())
}

fn spend_entity_budget(
    budget: &Cell<usize>,
    amount: usize,
    offset: usize,
) -> Result<(), ParseError> {
    let remaining = budget.get();
    let Some(next) = remaining.checked_sub(amount) else {
        return Err(error(offset, "XML entity expansion limit exceeded"));
    };
    budget.set(next);
    Ok(())
}

fn spend_entity_expansion(budget: &Cell<usize>, offset: usize) -> Result<(), ParseError> {
    let remaining = budget.get();
    let Some(next) = remaining.checked_sub(1) else {
        return Err(error(offset, "XML entity expansion count exceeded"));
    };
    budget.set(next);
    Ok(())
}

fn decode_attribute_entities(
    input: &str,
    offset: usize,
    html_catalog: bool,
    entities: &[GeneralEntity],
    budget: &Cell<usize>,
    expansions: &Cell<usize>,
    stack: &mut Vec<usize>,
) -> Result<String, ParseError> {
    if !input.contains('&') {
        return Ok(String::from(input));
    }
    let mut output = lumen_common::limits::size::string_with_capacity(input.len(), MAX_XML_BYTES)
        .map_err(|_| error(offset, "XML entity output too large"))?;
    let mut rest = input;
    let mut at = offset;
    while let Some(index) = rest.find('&') {
        append_entity_text(&mut output, &rest[..index], at)?;
        let after = &rest[index + 1..];
        let Some(end) = after.find(';') else {
            return Err(error(at + index, "unterminated entity reference"));
        };
        let name = &after[..end];
        if name.is_empty() {
            return Err(error(at + index, "empty entity reference"));
        }
        if name.len() > MAX_ENTITY_NAME_BYTES {
            return Err(error(at + index, "XML entity name too large"));
        }
        let reference_at = at + index;
        let token_start = index;
        let token_end = index + end + 2;
        let token = &rest[token_start..token_end];
        if is_predefined_or_character_reference(name) {
            let decoded = decode_entities(token, reference_at, false)?;
            append_entity_text(&mut output, &decoded, reference_at)?;
        } else if let Some(entity_index) = find_general_entity(entities, name) {
            let entity = &entities[entity_index];
            let Some(replacement) = entity.replacement.as_deref() else {
                return Err(error(
                    reference_at,
                    "external entity reference is unavailable",
                ));
            };
            expand_attribute_entity(
                entity_index,
                replacement,
                reference_at,
                html_catalog,
                entities,
                budget,
                expansions,
                stack,
                &mut output,
            )?;
        } else if html_catalog {
            let decoded = decode_entities(token, reference_at, true)?;
            append_entity_text(&mut output, &decoded, reference_at)?;
        } else {
            return Err(error(reference_at, "unknown entity reference"));
        }
        let used = index + end + 2;
        at += used;
        rest = &rest[used..];
    }
    append_entity_text(&mut output, rest, at)?;
    Ok(output)
}

fn expand_attribute_entity(
    entity_index: usize,
    replacement: &str,
    offset: usize,
    html_catalog: bool,
    entities: &[GeneralEntity],
    budget: &Cell<usize>,
    expansions: &Cell<usize>,
    stack: &mut Vec<usize>,
    output: &mut String,
) -> Result<(), ParseError> {
    if stack.len() >= MAX_ENTITY_DEPTH {
        return Err(error(offset, "XML entity nesting limit exceeded"));
    }
    if stack.contains(&entity_index) {
        return Err(error(offset, "recursive XML entity reference"));
    }
    spend_entity_expansion(expansions, offset)?;
    if entity_value_contains_less_than(replacement, offset)? {
        return Err(error(
            offset,
            "less-than sign in attribute entity replacement",
        ));
    }
    spend_entity_budget(budget, replacement.len(), offset)?;
    stack.push(entity_index);
    let decoded = decode_attribute_entities(
        replacement,
        offset,
        html_catalog,
        entities,
        budget,
        expansions,
        stack,
    );
    stack.pop();
    let decoded = decoded?;
    append_entity_text(output, &decoded, offset)
}

fn entity_value_contains_less_than(input: &str, offset: usize) -> Result<bool, ParseError> {
    if input.contains('<') {
        return Ok(true);
    }
    let mut rest = input;
    let mut at = offset;
    while let Some(index) = rest.find('&') {
        let after = &rest[index + 1..];
        let Some(end) = after.find(';') else {
            return Err(error(at + index, "unterminated DTD entity reference"));
        };
        let name = &after[..end];
        if name.starts_with('#') {
            let token_end = index + end + 2;
            let decoded = decode_entities(&rest[index..token_end], at + index, false)?;
            if decoded.contains('<') {
                return Ok(true);
            }
        }
        let used = index + end + 2;
        at += used;
        rest = &rest[used..];
    }
    Ok(false)
}

fn decode_entities(input: &str, offset: usize, html_catalog: bool) -> Result<String, ParseError> {
    if !input.contains('&') {
        return Ok(input.to_string());
    }
    let mut out = lumen_common::limits::size::string_with_capacity(input.len(), MAX_XML_BYTES)
        .map_err(|_| error(offset, "XML entity output too large"))?;
    let mut rest = input;
    let mut at = offset;
    while let Some(index) = rest.find('&') {
        append_entity_text(&mut out, &rest[..index], at)?;
        let after = &rest[index + 1..];
        let Some(end) = after.find(';') else {
            return Err(error(at + index, "unterminated entity reference"));
        };
        let entity = &after[..end];
        let ch = match entity {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "apos" => '\'',
            "quot" => '"',
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .filter(|ch| xml_char(*ch))
                    .ok_or_else(|| error(at + index, "invalid character reference"))?
            }
            value if value.starts_with('#') => value[1..]
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .filter(|ch| xml_char(*ch))
                .ok_or_else(|| error(at + index, "invalid character reference"))?,
            _ if html_catalog => {
                let candidate = &after.as_bytes()[..=end];
                let Some((consumed, replacement)) = crate::html::longest_entity(candidate)
                    .filter(|(consumed, _)| *consumed == candidate.len())
                else {
                    return Err(error(at + index, "unknown entity reference"));
                };
                append_entity_text(&mut out, replacement, at + index)?;
                let used = index + consumed + 1;
                at += used;
                rest = &rest[used..];
                continue;
            }
            _ => return Err(error(at + index, "unknown entity reference")),
        };
        append_entity_text(&mut out, ch.encode_utf8(&mut [0; 4]), at + index)?;
        let used = index + end + 2;
        at += used;
        rest = &rest[used..];
    }
    append_entity_text(&mut out, rest, at)?;
    Ok(out)
}

fn append_entity_text(out: &mut String, text: &str, offset: usize) -> Result<(), ParseError> {
    lumen_common::limits::size::append_string(out, text, MAX_XML_BYTES)
        .map_err(|_| error(offset, "XML entity output too large"))
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

const XML_NAMESPACE_URI: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NAMESPACE_URI: &str = "http://www.w3.org/2000/xmlns/";

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
    let mut bindings = alloc::vec![(String::from("xml"), String::from(XML_NAMESPACE_URI)),];
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
                                || !system_id.chars().all(xml_char)
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
                                    if value == XML_NAMESPACE_URI {
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
                                    || (prefix.is_empty() && value == XML_NAMESPACE_URI)
                                    || (!prefix.is_empty() && value == XML_NAMESPACE_URI)
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
                                    && (value == XMLNS_NAMESPACE_URI
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
                                if uri.is_some_and(|uri| uri == XMLNS_NAMESPACE_URI) {
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
    if uri != Some(XMLNS_NAMESPACE_URI) {
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
        && uri != XMLNS_NAMESPACE_URI
        && (prefix == "xml") == (uri == XML_NAMESPACE_URI)
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
        let name = if namespace == Some(XML_NAMESPACE_URI) {
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
            if has_local_default_namespace && local_default_namespace != Some(XML_NAMESPACE_URI) {
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
            if has_local_default_namespace && local_default_namespace != Some(XML_NAMESPACE_URI) {
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
    if input.chars().all(xml_char) {
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
        if require_well_formed && !xml_char(ch) {
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
    if require_well_formed && value.chars().any(|ch| !xml_char(ch)) {
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
    fn repeated_plain_entity_references_expand_with_bounded_text_storage() {
        let references = "&letter;".repeat(4096);
        let source = alloc::format!("<!DOCTYPE r [<!ENTITY letter 'x'>]><r>{references}</r>");
        let document = parse(&source, 8).unwrap();
        let root = root_element(&document);
        let text = document.first_child(root).unwrap().unwrap();
        assert_eq!(
            document.kind(text).unwrap(),
            &NodeKind::Text("x".repeat(4096))
        );
        assert!(document.next_sibling(text).unwrap().is_none());
    }

    #[test]
    fn empty_entity_expansions_are_counted_across_recursive_references() {
        let mut source = String::from("<!DOCTYPE r [<!ENTITY e0 ''>");
        for index in 1..=16 {
            let previous = (index - 1).to_string();
            let current = index.to_string();
            source.push_str("<!ENTITY e");
            source.push_str(&current);
            source.push_str(" '&e");
            source.push_str(&previous);
            source.push_str(";&e");
            source.push_str(&previous);
            source.push_str(";'>");
        }
        source.push_str("]><r>&e16;</r>");

        let failure = match parse(&source, 16) {
            Err(failure) => failure,
            Ok(_) => panic!("accepted excessive empty entity expansion"),
        };
        assert_eq!(failure.message, "XML entity expansion count exceeded");
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
    fn internal_entity_recursion_depth_and_expansion_are_bounded() {
        for source in [
            "<!DOCTYPE r [<!ENTITY loop '&loop;'>]><r>&loop;</r>",
            "<!DOCTYPE r [<!ENTITY one '&two;'><!ENTITY two '&one;'>]><r>&one;</r>",
        ] {
            assert!(
                parse(source, 64).is_err(),
                "accepted recursive entity: {source}"
            );
        }

        let mut deep = String::from("<!DOCTYPE r [<!ENTITY e0 'x'>");
        for index in 1..=MAX_ENTITY_DEPTH + 2 {
            let previous = (index - 1).to_string();
            let current = index.to_string();
            deep.push_str("<!ENTITY e");
            deep.push_str(&current);
            deep.push_str(" '&e");
            deep.push_str(&previous);
            deep.push_str(";'>");
        }
        deep.push_str("]><r>&e");
        deep.push_str(&(MAX_ENTITY_DEPTH + 2).to_string());
        deep.push_str(";</r>");
        assert!(parse(&deep, 64).is_err(), "accepted over-deep entity chain");

        let mut expanding = String::from("<!DOCTYPE r [<!ENTITY e0 'x'>");
        for index in 1..=24 {
            let previous = (index - 1).to_string();
            let current = index.to_string();
            expanding.push_str("<!ENTITY e");
            expanding.push_str(&current);
            expanding.push_str(" '&e");
            expanding.push_str(&previous);
            expanding.push_str(";&e");
            expanding.push_str(&previous);
            expanding.push_str(";'>");
        }
        expanding.push_str("]><r>&e24;</r>");
        assert!(
            parse(&expanding, 64).is_err(),
            "accepted over-budget expansion"
        );
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
    fn parameter_entity_references_are_rejected_without_resolution() {
        for source in [
            "<!DOCTYPE r [<!ENTITY % remote SYSTEM 'file:///never-read.dtd'>%remote;]><r/>",
            "<!DOCTYPE r [<!ENTITY % local '<!ENTITY value \"expanded\">'>%local;]><r/>",
            "<!DOCTYPE r [<!ENTITY value '%remote;'>]><r>&value;</r>",
        ] {
            assert!(
                parse(source, 16).is_err(),
                "accepted an unresolved parameter-entity reference: {source}"
            );
        }
    }

    #[test]
    fn xml_catalog_expansion_checks_output_budget_before_growth() {
        let source = "&nLt;".repeat(MAX_XML_BYTES / 6 + 1);
        assert!(source.len() < MAX_XML_BYTES);
        let error = decode_entities(&source, 0, true).unwrap_err();
        assert_eq!(error.message, "XML entity output too large");
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
    fn namespace_attribute_prefix_is_preserved_when_value_changes() {
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
        let attribute = document
            .attribute_node_by_ns(element, Some("urn:keys"), "key")
            .unwrap()
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
        assert_eq!(
            document.attribute_node_by_ns(element, Some("urn:keys"), "key"),
            Ok(Some(attribute))
        );
        assert!(matches!(
            document.kind(attribute).unwrap(),
            crate::NodeKind::Attribute { qualified_name, value, .. }
                if qualified_name == "x:key" && value == "new"
        ));
        assert_eq!(
            document.attribute_node_by_name(element, "y:key").unwrap(),
            None
        );
    }
}
