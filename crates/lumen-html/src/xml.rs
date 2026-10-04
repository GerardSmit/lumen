//! Bounded, non-validating XML parsing into the shared document arena.
//!
//! External identifiers are retained only as doctype text; this parser never
//! loads external subsets or entities. DTD-defined entities are not expanded.
use crate::{Document, Error as DomError, Name, Namespace, NodeId, NodeKind};
use alloc::{
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

const MAX_XML_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 512;

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
    if let Some((offset, _)) = input.char_indices().find(|(_, ch)| !xml_char(*ch)) {
        return Err(error(offset, "invalid XML character"));
    }
    let mut parser = Parser {
        input,
        pos: 0,
        document: Document::new(max_nodes),
        roots: 0,
        depth: 0,
        doctype_name: None,
        root_name: None,
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

struct Parser<'a> {
    input: &'a str,
    pos: usize,
    document: Document,
    roots: usize,
    depth: usize,
    doctype_name: Option<String>,
    root_name: Option<String>,
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
        self.doctype_name = Some(name);
        let mut quote = None;
        let mut subset = 0usize;
        while !self.eof() {
            let ch = self.input[self.pos..].chars().next().unwrap();
            self.pos += ch.len_utf8();
            if let Some(q) = quote {
                if ch == q {
                    quote = None;
                }
                continue;
            }
            match ch {
                '\'' | '"' => quote = Some(ch),
                '[' => subset += 1,
                ']' if subset > 0 => subset -= 1,
                '>' if subset == 0 => {
                    let raw = self.input[start..self.pos].to_string();
                    let id = self.create(NodeKind::DocumentType(raw))?;
                    self.document
                        .append(parent, id)
                        .map_err(|_| error(start, "invalid doctype placement"))?;
                    return Ok(());
                }
                _ => {}
            }
        }
        Err(error(start, "unterminated doctype"))
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
                self.comment(id)?;
            } else if self.starts("<?") {
                self.processing_instruction_for(id)?;
            } else if self.starts("<![CDATA[") {
                self.pos += 9;
                let data_at = self.pos;
                let Some(end) = self.input[self.pos..].find("]]>") else {
                    return Err(error(data_at, "unterminated CDATA section"));
                };
                let text = String::from(&self.input[data_at..data_at + end]);
                self.pos = data_at + end + 3;
                self.text(id, text)?;
            } else if self.starts("<!") {
                return Err(error(self.pos, "unsupported markup declaration"));
            } else if self.starts("<") {
                self.element(id, namespaces.clone())?;
            } else {
                let text_at = self.pos;
                while !self.eof() && !self.starts("<") {
                    self.bump_char();
                }
                let raw = &self.input[text_at..self.pos];
                if raw.contains("]]>") {
                    return Err(error(text_at, "forbidden ]]> in character data"));
                }
                let text = decode_entities(raw, text_at)?;
                self.text(id, text)?;
            }
        }
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
        let value = decode_entities(&self.input[start..self.pos], start)?;
        self.pos += q.len_utf8();
        Ok(value)
    }
    fn text(&mut self, parent: NodeId, text: String) -> Result<(), ParseError> {
        if text.is_empty() {
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
fn decode_entities(input: &str, offset: usize) -> Result<String, ParseError> {
    if !input.contains('&') {
        return Ok(input.to_string());
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    let mut at = offset;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
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
            _ => return Err(error(at + index, "unknown entity reference")),
        };
        out.push(ch);
        let used = index + end + 2;
        at += used;
        rest = &rest[used..];
    }
    out.push_str(rest);
    Ok(out)
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
