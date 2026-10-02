//! HTML document ingestion into the shared DOM arena.
use crate::{Document, Error as DomError, Namespace, NodeId, NodeKind};
use alloc::{
    borrow::Cow,
    string::{String, ToString},
    vec::Vec,
};

const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

fn normalized_input(input: &str) -> Cow<'_, str> {
    if !input.contains(['\r', '\0']) {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\0' => out.push('\u{fffd}'),
            _ => out.push(ch),
        }
    }
    Cow::Owned(out)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    pub offset: usize,
    pub message: &'static str,
}

fn error(offset: usize, message: &'static str) -> ParseError {
    ParseError { offset, message }
}

fn dom_error(offset: usize, value: DomError) -> ParseError {
    error(
        offset,
        match value {
            DomError::LimitExceeded => "node limit exceeded",
            DomError::UnsupportedDoctype => "unsupported doctype",
            _ => "invalid document tree",
        },
    )
}

fn element(name: impl Into<String>, attributes: Vec<(String, String)>) -> NodeKind {
    NodeKind::Element {
        namespace: Namespace::Html,
        name: name.into(),
        attributes,
    }
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn is_formatting(name: &str) -> bool {
    matches!(
        name,
        "a" | "b" | "em" | "i" | "s" | "small" | "strong" | "u"
    )
}

fn is_special(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "body"
            | "br"
            | "button"
            | "caption"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hgroup"
            | "hr"
            | "html"
            | "img"
            | "input"
            | "li"
            | "link"
            | "main"
            | "menu"
            | "meta"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "script"
            | "section"
            | "select"
            | "source"
            | "style"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "template"
            | "textarea"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "ul"
            | "wbr"
    )
}

fn named_entity(name: &str) -> Option<&'static str> {
    let index = crate::entities::ENTRIES
        .binary_search_by(|&(start, len, _, _)| {
            crate::entities::NAMES[start as usize..start as usize + len as usize].cmp(name)
        })
        .ok()?;
    let (_, _, start, len) = crate::entities::ENTRIES[index];
    Some(&crate::entities::VALUES[start as usize..start as usize + len as usize])
}

fn numeric_entity(input: &str) -> Option<(char, usize)> {
    let rest = input.strip_prefix('#')?;
    let (radix, rest, prefix) =
        if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
            (16, hex, 2)
        } else {
            (10, rest, 1)
        };
    let digits = rest
        .bytes()
        .take_while(|byte| {
            if radix == 16 {
                byte.is_ascii_hexdigit()
            } else {
                byte.is_ascii_digit()
            }
        })
        .count();
    if digits == 0 {
        return None;
    }
    let mut value = u32::from_str_radix(&rest[..digits], radix).unwrap_or(0xfffd);
    const C1: [u32; 32] = [
        0x20ac, 0x81, 0x201a, 0x192, 0x201e, 0x2026, 0x2020, 0x2021, 0x2c6, 0x2030, 0x160, 0x2039,
        0x152, 0x8d, 0x17d, 0x8f, 0x90, 0x2018, 0x2019, 0x201c, 0x201d, 0x2022, 0x2013, 0x2014,
        0x2dc, 0x2122, 0x161, 0x203a, 0x153, 0x9d, 0x17e, 0x178,
    ];
    if (0x80..=0x9f).contains(&value) {
        value = C1[(value - 0x80) as usize];
    }
    let value = char::from_u32(value)
        .filter(|&value| value != '\0')
        .unwrap_or('\u{fffd}');
    let consumed = prefix + digits + usize::from(rest.as_bytes().get(digits) == Some(&b';'));
    Some((value, consumed))
}

fn decode_entities(input: &str, attribute: bool) -> String {
    let mut output = String::with_capacity(input.len());
    let mut remaining = input;
    while let Some(start) = remaining.find('&') {
        output.push_str(&remaining[..start]);
        let after_amp = &remaining[start + 1..];
        if let Some((value, consumed)) = numeric_entity(after_amp) {
            output.push(value);
            remaining = &after_amp[consumed..];
            continue;
        }
        let mut best = None;
        for end in 1..=after_amp.len().min(32) {
            let byte = after_amp.as_bytes()[end - 1];
            if !byte.is_ascii_alphanumeric() && byte != b';' {
                break;
            }
            if let Some(value) = named_entity(&after_amp[..end]) {
                best = Some((end, value));
            }
            if byte == b';' {
                break;
            }
        }
        if let Some((end, value)) = best {
            if !(attribute
                && !after_amp[..end].ends_with(';')
                && after_amp
                    .as_bytes()
                    .get(end)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'='))
            {
                output.push_str(value);
                remaining = &after_amp[end..];
                continue;
            }
        }
        output.push('&');
        remaining = after_amp;
    }
    output.push_str(remaining);
    output
}

fn escape(output: &mut String, input: &str, attribute: bool) {
    for character in input.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' if attribute => output.push_str("&quot;"),
            _ => output.push(character),
        }
    }
}

fn serialize_into(
    document: &Document,
    root: NodeId,
    children_only: bool,
    output: &mut String,
) -> Result<(), DomError> {
    let mut pending = Vec::new();
    if children_only {
        let raw = matches!(document.kind(root)?, NodeKind::Element { name, .. } if matches!(name.as_str(), "style" | "script"));
        let mut child = document.last_child(document.template_content(root)?.unwrap_or(root))?;
        while let Some(id) = child {
            pending.push((id, false, raw));
            child = document.previous_sibling(id)?;
        }
    } else {
        pending.push((root, false, false));
    }
    while let Some((id, closing, raw_text)) = pending.pop() {
        let kind = document.kind(id)?;
        if closing {
            if let NodeKind::Element { name, .. } = kind {
                output.push_str("</");
                output.push_str(name);
                output.push('>');
            }
            continue;
        }
        let mut descend = true;
        let mut child_raw = raw_text;
        match kind {
            NodeKind::Document | NodeKind::DocumentFragment => {}
            NodeKind::DocumentType(name) => {
                output.push_str("<!DOCTYPE ");
                output.push_str(name);
                output.push('>');
                descend = false;
            }
            NodeKind::Element {
                name, attributes, ..
            } => {
                output.push('<');
                output.push_str(name);
                for (key, value) in attributes {
                    output.push(' ');
                    output.push_str(key);
                    output.push_str("=\"");
                    escape(output, value, true);
                    output.push('"');
                }
                output.push('>');
                descend = !is_void(name);
                child_raw = matches!(name.as_str(), "style" | "script");
                if descend {
                    pending.push((id, true, raw_text));
                }
            }
            NodeKind::Text(value) => {
                if raw_text {
                    output.push_str(value);
                } else {
                    escape(output, value, false);
                }
                descend = false;
            }
            NodeKind::Comment(value) => {
                output.push_str("<!--");
                output.push_str(value);
                output.push_str("-->");
                descend = false;
            }
            NodeKind::ProcessingInstruction { target, data } => {
                output.push_str("<?");
                output.push_str(target);
                output.push(' ');
                output.push_str(data);
                output.push_str("?>");
                descend = false;
            }
        }
        if descend {
            let mut child = document.last_child(document.template_content(id)?.unwrap_or(id))?;
            while let Some(next) = child {
                pending.push((next, false, child_raw));
                child = document.previous_sibling(next)?;
            }
        }
    }
    Ok(())
}

pub fn outer_html(document: &Document, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(document, id, false, &mut output)?;
    Ok(output)
}

pub fn inner_html(document: &Document, id: NodeId) -> Result<String, DomError> {
    let mut output = String::new();
    serialize_into(document, id, true, &mut output)?;
    Ok(output)
}

/// Parse an application document. Scripts are retained as inert text.
pub fn parse(input: &str, max_nodes: usize) -> Result<Document, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let input = normalized_input(input);
    let mut document = Document::new(max_nodes);
    let root = document.root();
    let html = document
        .create(element("html", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let head = document
        .create(element("head", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    let body = document
        .create(element("body", Vec::new()))
        .map_err(|e| dom_error(0, e))?;
    document.attach_detached(root, html);
    document.attach_detached(html, head);
    document.attach_detached(html, body);
    Parser {
        input: &input,
        pos: 0,
        document: &mut document,
        html: Some(html),
        head: Some(head),
        body,
        stack: Vec::new(),
        formatting: Vec::new(),
        in_head: false,
        body_started: false,
    }
    .run()?;
    Ok(document)
}

/// Parse a detached fragment in an existing arena for template reuse.
pub fn parse_fragment(document: &mut Document, input: &str) -> Result<NodeId, ParseError> {
    parse_fragment_context(document, input, None)
}

/// Parse markup using the tokenizer and table context of an HTML element.
pub fn parse_fragment_in(
    document: &mut Document,
    context: NodeId,
    input: &str,
) -> Result<NodeId, ParseError> {
    let kind = match document.kind(context).map_err(|e| dom_error(0, e))? {
        NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        } => element(name.clone(), Vec::new()),
        _ => return Err(error(0, "fragment context must be an HTML element")),
    };
    parse_fragment_context(document, input, Some(kind))
}

fn parse_fragment_context(
    document: &mut Document,
    input: &str,
    context: Option<NodeKind>,
) -> Result<NodeId, ParseError> {
    if input.len() > MAX_HTML_BYTES {
        return Err(error(0, "HTML input too large"));
    }
    let input = normalized_input(input);
    let fragment = document
        .create(NodeKind::DocumentFragment)
        .map_err(|e| dom_error(0, e))?;
    if let Some(NodeKind::Element { name, .. }) = &context {
        if matches!(name.as_str(), "style" | "script" | "title" | "textarea") {
            let value = if matches!(name.as_str(), "title" | "textarea") {
                decode_entities(&input, false)
            } else {
                input.to_string()
            };
            if !value.is_empty() {
                match document.create(NodeKind::Text(value)) {
                    Ok(text) => document.attach_detached(fragment, text),
                    Err(e) => {
                        document
                            .destroy_subtree(fragment)
                            .map_err(|e| dom_error(0, e))?;
                        return Err(dom_error(0, e));
                    }
                }
            }
            return Ok(fragment);
        }
    }
    let context = match context {
        Some(kind) => match document.create(kind) {
            Ok(id) => {
                document.attach_detached(fragment, id);
                Some(id)
            }
            Err(e) => {
                document
                    .destroy_subtree(fragment)
                    .map_err(|e| dom_error(0, e))?;
                return Err(dom_error(0, e));
            }
        },
        None => None,
    };
    let mut stack = Vec::new();
    if let Some(context) = context {
        stack.push(context);
    }
    let result = Parser {
        input: &input,
        pos: 0,
        document,
        html: None,
        head: None,
        body: fragment,
        stack,
        formatting: Vec::new(),
        in_head: false,
        body_started: false,
    }
    .run();
    match result {
        Ok(()) => {
            if let Some(context) = context {
                let source = document.template_content(context).map_err(|e| dom_error(0, e))?.unwrap_or(context);
                while let Some(child) =
                    document.first_child(source).map_err(|e| dom_error(0, e))?
                {
                    document.detach(child).map_err(|e| dom_error(0, e))?;
                    document.insert_detached_before(fragment, child, Some(context));
                }
                document.detach(context).map_err(|e| dom_error(0, e))?;
                document
                    .destroy_subtree(context)
                    .map_err(|e| dom_error(0, e))?;
            }
            Ok(fragment)
        }
        Err(error) => {
            document
                .destroy_subtree(fragment)
                .map_err(|e| dom_error(0, e))?;
            Err(error)
        }
    }
}

struct Parser<'a, 'd> {
    input: &'a str,
    pos: usize,
    document: &'d mut Document,
    html: Option<NodeId>,
    head: Option<NodeId>,
    body: NodeId,
    stack: Vec<NodeId>,
    formatting: Vec<Option<NodeId>>,
    in_head: bool,
    body_started: bool,
}

impl Parser<'_, '_> {
    fn raw_text_end(&self, name: &str) -> usize {
        let bytes = self.remaining().as_bytes();
        let appropriate = |index: usize, prefix: &[u8]| {
            bytes
                .get(index..index + prefix.len())
                .is_some_and(|value| value.eq_ignore_ascii_case(prefix))
                && bytes
                    .get(index + prefix.len())
                    .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
        };
        let mut script_state = 0;
        for index in 0..bytes.len() {
            if name == "script" {
                if script_state == 0 && bytes[index..].starts_with(b"<!--") {
                    script_state = 1;
                } else if script_state != 0 && bytes[index..].starts_with(b"-->") {
                    script_state = 0;
                } else if script_state == 1 && appropriate(index, b"<script") {
                    script_state = 2;
                } else if script_state == 2 && appropriate(index, b"</script") {
                    script_state = 1;
                    continue;
                }
            }
            if script_state != 2
                && bytes[index..].starts_with(b"</")
                && bytes
                    .get(index + 2..index + 2 + name.len())
                    .is_some_and(|value| value.eq_ignore_ascii_case(name.as_bytes()))
                && bytes
                    .get(index + 2 + name.len())
                    .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
            {
                return index;
            }
        }
        bytes.len()
    }

    fn reconstruct_formatting(&mut self) -> Result<(), ParseError> {
        let mut index = self.formatting.len();
        while index > 0 {
            match self.formatting[index - 1] {
                Some(id) if !self.stack.contains(&id) => index -= 1,
                _ => break,
            }
        }
        while index < self.formatting.len() {
            let old = self.formatting[index].unwrap();
            let kind = self
                .document
                .kind(old)
                .map_err(|e| dom_error(self.pos, e))?
                .clone();
            let id = self
                .document
                .create(kind)
                .map_err(|e| dom_error(self.pos, e))?;
            let (parent, before) = self.insertion_location(self.parent());
            self.document.insert_detached_before(parent, id, before);
            self.stack.push(id);
            self.formatting[index] = Some(id);
            index += 1;
        }
        Ok(())
    }

    fn end_formatting(&mut self, name: &str) -> Result<(), ParseError> {
        for _ in 0..8 {
            let Some(index) = self.formatting.iter().rposition(|entry| match entry {
            Some(id) => matches!(self.document.kind(*id), Ok(NodeKind::Element { name: open, .. }) if open == name),
            None => false,
        }) else {
            self.close_open(name);
            return Ok(());
        };
            if self.formatting[index + 1..].contains(&None) {
                return Ok(());
            }
            let id = self.formatting[index].unwrap();
            let Some(open) = self.stack.iter().position(|&node| node == id) else {
                self.formatting.remove(index);
                return Ok(());
            };
            if self.stack[open + 1..].iter().any(|&node| matches!(self.document.kind(node), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "table" | "td" | "th" | "template"))) {
            return Ok(());
        }
            let block = self.stack[open + 1..].iter().position(|&node| matches!(self.document.kind(node), Ok(NodeKind::Element { name, .. }) if is_special(name))).map(|position| open + 1 + position);
            let Some(block) = block else {
                self.stack.truncate(open);
                self.formatting.remove(index);
                return Ok(());
            };
            let ancestor = if open == 0 {
                self.body
            } else {
                self.stack[open - 1]
            };
            let furthest = self.stack[block];
            let mut last = furthest;
            let mut bookmark = index;
            for (counter, position) in (open + 1..block).rev().enumerate() {
                let node = self.stack[position];
                let active = self
                    .formatting
                    .iter()
                    .position(|&entry| entry == Some(node));
                let active = if counter >= 3 {
                    if let Some(active) = active {
                        self.formatting.remove(active);
                        if active < bookmark {
                            bookmark -= 1;
                        }
                    }
                    None
                } else {
                    active
                };
                let Some(active) = active else {
                    self.stack.remove(position);
                    continue;
                };
                let kind = self
                    .document
                    .kind(node)
                    .map_err(|e| dom_error(self.pos, e))?
                    .clone();
                let copy = self
                    .document
                    .create(kind)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(ancestor, copy);
                self.formatting[active] = Some(copy);
                self.stack[position] = copy;
                if last == furthest {
                    bookmark = active + 1;
                }
                self.document
                    .detach(last)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(copy, last);
                last = copy;
            }
            self.document
                .detach(last)
                .map_err(|e| dom_error(self.pos, e))?;
            let (parent, before) = self.insertion_location(ancestor);
            self.document.insert_detached_before(parent, last, before);
            let kind = self
                .document
                .kind(id)
                .map_err(|e| dom_error(self.pos, e))?
                .clone();
            let copy = self
                .document
                .create(kind)
                .map_err(|e| dom_error(self.pos, e))?;
            while let Some(child) = self
                .document
                .first_child(furthest)
                .map_err(|e| dom_error(self.pos, e))?
            {
                self.document
                    .detach(child)
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(copy, child);
            }
            self.append(furthest, copy);
            let active = self
                .formatting
                .iter()
                .position(|&entry| entry == Some(id))
                .unwrap();
            self.formatting.remove(active);
            if active < bookmark {
                bookmark -= 1;
            }
            self.formatting.insert(bookmark, Some(copy));
            self.stack.remove(open);
            let block = self
                .stack
                .iter()
                .position(|&node| node == furthest)
                .unwrap();
            self.stack.insert(block + 1, copy);
        }
        Ok(())
    }
    fn remaining(&self) -> &str {
        &self.input[self.pos..]
    }
    fn parent(&self) -> NodeId {
        self.stack.last().map_or(
            if self.in_head {
                self.head.unwrap_or(self.body)
            } else {
                self.body
            },
            |id| *id,
        )
    }

    fn append(&mut self, parent: NodeId, child: NodeId) {
        let parent = self.document.template_content(parent).ok().flatten().unwrap_or(parent);
        self.document.attach_detached(parent, child);
    }

    fn append_text(
        &mut self,
        parent: NodeId,
        value: String,
        offset: usize,
    ) -> Result<(), ParseError> {
        if value.is_empty() {
            return Ok(());
        }
        let parent = if !value.bytes().all(|byte| byte.is_ascii_whitespace())
            && matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if name == "colgroup")
        {
            self.close_open("colgroup");
            self.parent()
        } else {
            parent
        };
        let (parent, before) = if !value.bytes().all(|byte| byte.is_ascii_whitespace()) {
            self.insertion_location(parent)
        } else {
            (self.document.template_content(parent).ok().flatten().unwrap_or(parent), None)
        };
        let previous = if let Some(before) = before {
            self.document.previous_sibling(before)
        } else {
            self.document.last_child(parent)
        }
        .map_err(|e| dom_error(offset, e))?;
        if let Some(last) = previous {
            if let NodeKind::Text(existing) = &mut self.document.node_mut(last).kind {
                existing.push_str(&value);
                return Ok(());
            }
        }
        let id = self
            .document
            .create(NodeKind::Text(value))
            .map_err(|e| dom_error(offset, e))?;
        self.document.insert_detached_before(parent, id, before);
        Ok(())
    }

    fn insertion_location(&self, parent: NodeId) -> (NodeId, Option<NodeId>) {
        if let Ok(Some(content)) = self.document.template_content(parent) { return (content, None); }
        if matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "table" | "tbody" | "thead" | "tfoot" | "tr"))
        {
            if let Some(&table) = self.stack.iter().rev().find(|&&id| matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "table")) {
                if let Ok(Some(parent)) = self.document.parent(table) { return (parent, Some(table)); }
            }
        }
        (parent, None)
    }

    fn close_open(&mut self, name: &str) -> bool {
        if let Some(index) = self.stack.iter().rposition(|&id| {
            matches!(self.document.kind(id), Ok(NodeKind::Element { name: open, .. }) if open == name)
        }) {
            let markers = self.stack[index..].iter().filter(|&&id| matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "td" | "th" | "template" | "caption"))).count();
            for _ in 0..markers { self.clear_formatting(); }
            self.stack.truncate(index);
            true
        } else {
            false
        }
    }

    fn clear_formatting(&mut self) {
        while let Some(entry) = self.formatting.pop() {
            if entry.is_none() {
                break;
            }
        }
    }

    fn clear_to_table_context(&mut self, names: &[&str]) {
        while self.stack.last().is_some_and(|&id| !matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if names.contains(&name.as_str()) || name == "template")) {
            self.stack.pop();
        }
    }

    fn close_in_scope(&mut self, names: &[&str], boundary: &[&str]) {
        for index in (0..self.stack.len()).rev() {
            let Ok(NodeKind::Element { name, .. }) = self.document.kind(self.stack[index]) else {
                break;
            };
            if names.iter().any(|candidate| *candidate == name) {
                if matches!(name.as_str(), "td" | "th" | "template" | "caption") {
                    self.clear_formatting();
                }
                self.stack.truncate(index);
                return;
            }
            if boundary.iter().any(|candidate| *candidate == name) {
                return;
            }
        }
    }

    fn merge_scaffold_attributes(&mut self, id: NodeId, attributes: Vec<(String, String)>) {
        if let NodeKind::Element {
            attributes: existing,
            ..
        } = &mut self.document.node_mut(id).kind
        {
            for (key, value) in attributes {
                if !existing.iter().any(|(name, _)| name == &key) {
                    existing.push((key, value));
                }
            }
        }
    }

    fn run(&mut self) -> Result<(), ParseError> {
        while self.pos < self.input.len() {
            if let Some(&parent) = self.stack.last() {
                let NodeKind::Element { name, .. } = self
                    .document
                    .kind(parent)
                    .map_err(|e| dom_error(self.pos, e))?
                else {
                    return Err(error(self.pos, "invalid parser stack"));
                };
                if matches!(name.as_str(), "style" | "script" | "title" | "textarea") {
                    let rcdata = matches!(name.as_str(), "title" | "textarea");
                    let end = self.raw_text_end(name);
                    if end > 0 {
                        let raw = &self.remaining()[..end];
                        let text = if rcdata {
                            decode_entities(raw, false)
                        } else {
                            raw.to_string()
                        };
                        self.append_text(parent, text, self.pos)?;
                        self.pos += end;
                        continue;
                    }
                }
            }
            if self.remaining().starts_with("<!--") {
                self.comment()?;
            } else if self.remaining().len() >= 9
                && self.remaining().as_bytes()[..9].eq_ignore_ascii_case(b"<!doctype")
            {
                self.doctype()?;
            } else if self.remaining().starts_with("</") {
                self.end_tag()?;
            } else if self.remaining().starts_with("<!") || self.remaining().starts_with("<?") {
                let start = self.pos + 2;
                let end = self.input[start..]
                    .find('>')
                    .map_or(self.input.len(), |end| start + end);
                let id = self
                    .document
                    .create(NodeKind::Comment(self.input[start..end].to_string()))
                    .map_err(|e| dom_error(self.pos, e))?;
                self.append(self.parent(), id);
                self.pos = (end + 1).min(self.input.len());
            } else if self.remaining().starts_with('<')
                && self
                    .remaining()
                    .as_bytes()
                    .get(1)
                    .is_some_and(u8::is_ascii_alphabetic)
            {
                self.start_tag()?;
            } else {
                self.text()?;
            }
        }
        Ok(())
    }

    fn comment(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        let rest = &self.input[start + 4..];
        let ending = rest
            .find("-->")
            .map(|end| (end, 3))
            .into_iter()
            .chain(rest.find("--!>").map(|end| (end, 4)))
            .min_by_key(|&(end, _)| end);
        let (end, suffix) = ending.unwrap_or((rest.len(), 0));
        let id = self
            .document
            .create(NodeKind::Comment(rest[..end].to_string()))
            .map_err(|e| dom_error(start, e))?;
        self.append(self.parent(), id);
        self.pos = start + 4 + end + suffix;
        Ok(())
    }

    fn doctype(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.html
            .ok_or_else(|| error(start, "doctype is not allowed in fragments"))?;
        let end = self
            .remaining()
            .find('>')
            .ok_or_else(|| error(start, "unterminated doctype"))?;
        if !self.remaining()[9..end].trim().eq_ignore_ascii_case("html") {
            return Err(error(start, "unsupported doctype"));
        }
        let id = self
            .document
            .create(NodeKind::DocumentType("html".to_string()))
            .map_err(|e| dom_error(start, e))?;
        self.document.prepend_detached(self.document.root(), id);
        self.pos += end + 1;
        Ok(())
    }

    fn end_tag(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.pos += 2;
        if self.remaining().starts_with('>') {
            self.pos += 1;
            return Ok(());
        }
        if !self
            .remaining()
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        {
            let end = self.remaining().find('>').unwrap_or(self.remaining().len());
            if end > 0 {
                let id = self
                    .document
                    .create(NodeKind::Comment(self.remaining()[..end].to_string()))
                    .map_err(|e| dom_error(start, e))?;
                self.append(self.parent(), id);
            }
            self.pos = (self.pos + end + 1).min(self.input.len());
            return Ok(());
        }
        let name = self.name()?;
        let Some(end) = self.remaining().find('>') else {
            self.pos = self.input.len();
            return Ok(());
        };
        self.pos += end + 1;
        if name == "br" {
            self.reconstruct_formatting()?;
            let id = self
                .document
                .create(element("br", Vec::new()))
                .map_err(|e| dom_error(start, e))?;
            let (parent, before) = self.insertion_location(self.parent());
            self.document.insert_detached_before(parent, id, before);
            return Ok(());
        }
        if name == "p" && !self.stack.iter().any(|&id| matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "p")) {
            let id = self.document.create(element("p", Vec::new())).map_err(|e| dom_error(start, e))?;
            self.append(self.parent(), id);
            return Ok(());
        }
        if self.html.is_some() && name == "head" {
            self.in_head = false;
            self.body_started = true;
            self.stack.clear();
            return Ok(());
        }
        if self.html.is_some() && (name == "body" || name == "html") {
            self.body_started = true;
            self.stack.clear();
            return Ok(());
        }
        if is_formatting(&name) {
            self.end_formatting(&name)?;
        } else {
            self.close_open(&name);
        }
        Ok(())
    }

    fn start_tag(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        self.pos += 1;
        let name = self.name()?;
        if matches!(name.as_str(), "svg" | "math") {
            return Err(error(start, "foreign content is unsupported"));
        }
        if matches!(
            name.as_str(),
            "applet"
                | "basefont"
                | "bgsound"
                | "big"
                | "blink"
                | "center"
                | "dir"
                | "font"
                | "frame"
                | "frameset"
                | "isindex"
                | "keygen"
                | "listing"
                | "marquee"
                | "menuitem"
                | "nobr"
                | "noembed"
                | "noframes"
                | "plaintext"
                | "strike"
                | "tt"
                | "xmp"
        ) {
            return Err(error(start, "obsolete element"));
        }
        let mut attributes = Vec::new();
        let _self_closing = loop {
            self.skip_space();
            if self.remaining().is_empty() {
                return Ok(());
            }
            if self.remaining().starts_with("/>") {
                self.pos += 2;
                break true;
            }
            if self.remaining().starts_with('>') {
                self.pos += 1;
                break false;
            }
            if self.remaining().starts_with('/') {
                self.pos += 1;
                continue;
            }
            if self.pos >= self.input.len() {
                return Err(error(start, "unterminated start tag"));
            }
            let attr = self.attribute_name()?;
            self.skip_space();
            let value = if self.remaining().starts_with('=') {
                self.pos += 1;
                self.skip_space();
                self.attribute_value()?
            } else {
                String::new()
            };
            if !attributes.iter().any(|(key, _)| key == &attr) {
                attributes.push((attr, value));
            }
        };
        if self.html.is_some() && name == "html" {
            self.merge_scaffold_attributes(self.html.unwrap(), attributes);
            return Ok(());
        }
        if self.html.is_some() && name == "head" {
            self.merge_scaffold_attributes(self.head.unwrap(), attributes);
            self.in_head = true;
            return Ok(());
        }
        if self.html.is_some() && name == "body" {
            self.merge_scaffold_attributes(self.body, attributes);
            self.in_head = false;
            self.body_started = true;
            self.stack.clear();
            return Ok(());
        }
        if self.html.is_some() && self.stack.is_empty() && !self.body_started {
            if matches!(
                name.as_str(),
                "base" | "link" | "meta" | "style" | "script" | "title" | "noscript"
            ) {
                self.in_head = true;
            } else {
                self.in_head = false;
                self.body_started = true;
            }
        }
        if !matches!(name.as_str(), "col" | "template")
            && matches!(self.document.kind(self.parent()), Ok(NodeKind::Element { name, .. }) if name == "colgroup")
        {
            self.close_open("colgroup");
        }
        if matches!(
            name.as_str(),
            "caption" | "colgroup" | "col" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th"
        ) && matches!(self.document.kind(self.parent()), Ok(NodeKind::Element { name, .. }) if name == "caption")
        {
            self.close_open("caption");
        }
        if matches!(name.as_str(), "caption" | "colgroup" | "col" | "tbody" | "thead" | "tfoot" | "tr" | "td" | "th")
            && !self.stack.iter().any(|&id| matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "table" | "tbody" | "thead" | "tfoot" | "tr" | "template"))) {
            return Ok(());
        }
        if self.stack.iter().any(|&id| matches!(self.document.kind(id), Ok(NodeKind::Element { name, .. }) if name == "select")) {
            match name.as_str() {
                "option" => { self.close_in_scope(&["option"], &["select", "optgroup"]); }
                "optgroup" => { self.close_in_scope(&["option"], &["select"]); self.close_in_scope(&["optgroup"], &["select"]); }
                "select" => { self.close_open("select"); return Ok(()); }
                "input" | "textarea" => { self.close_open("select"); }
                "script" | "template" => {}
                _ => return Ok(()),
            }
        }
        if name == "table"
            && matches!(self.document.kind(self.parent()), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "table" | "tbody" | "thead" | "tfoot" | "tr"))
        {
            self.close_open("table");
        }
        if matches!(
            name.as_str(),
            "address"
                | "article"
                | "aside"
                | "blockquote"
                | "div"
                | "dl"
                | "fieldset"
                | "footer"
                | "form"
                | "h1"
                | "h2"
                | "h3"
                | "h4"
                | "h5"
                | "h6"
                | "header"
                | "hr"
                | "main"
                | "nav"
                | "ol"
                | "p"
                | "pre"
                | "section"
                | "table"
                | "ul"
        ) {
            self.close_open("p");
        }
        if name == "li" {
            self.close_in_scope(&["li"], &["ol", "ul"]);
        }
        if matches!(name.as_str(), "dt" | "dd") {
            self.close_in_scope(&["dt", "dd"], &["dl"]);
        }
        if matches!(name.as_str(), "tbody" | "thead" | "tfoot") {
            self.close_in_scope(&["td", "th"], &["table"]);
            self.close_in_scope(&["tr"], &["table"]);
            self.close_in_scope(&["tbody", "thead", "tfoot"], &["table"]);
            self.clear_to_table_context(&["table"]);
        }
        if name == "tr" {
            self.close_in_scope(&["td", "th"], &["tr", "table"]);
            self.close_in_scope(&["tr"], &["table"]);
            self.clear_to_table_context(&["table", "tbody", "thead", "tfoot"]);
            let parent = self.parent();
            if matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if name == "table")
            {
                let tbody = self
                    .document
                    .create(element("tbody", Vec::new()))
                    .map_err(|e| dom_error(start, e))?;
                self.append(parent, tbody);
                self.stack.push(tbody);
            }
        }
        if matches!(name.as_str(), "td" | "th") {
            self.close_in_scope(&["td", "th"], &["tr", "table"]);
            self.clear_to_table_context(&["table", "tbody", "thead", "tfoot", "tr"]);
            let parent = self.parent();
            if matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if name == "table")
            {
                let tbody = self
                    .document
                    .create(element("tbody", Vec::new()))
                    .map_err(|e| dom_error(start, e))?;
                self.append(parent, tbody);
                self.stack.push(tbody);
            }
            let parent = self.parent();
            if matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "tbody" | "thead" | "tfoot"))
            {
                let tr = self
                    .document
                    .create(element("tr", Vec::new()))
                    .map_err(|e| dom_error(start, e))?;
                self.append(parent, tr);
                self.stack.push(tr);
            }
        }
        if name == "col" {
            let parent = self.parent();
            if matches!(self.document.kind(parent), Ok(NodeKind::Element { name, .. }) if name == "table")
            {
                let group = self
                    .document
                    .create(element("colgroup", Vec::new()))
                    .map_err(|e| dom_error(start, e))?;
                self.append(parent, group);
                self.stack.push(group);
            }
        }
        if name == "a" {
            self.end_formatting("a")?;
        }
        let formatting = is_formatting(&name);
        if formatting || !is_special(&name) || matches!(name.as_str(), "img" | "br" | "input") {
            self.reconstruct_formatting()?;
        }
        let marker = matches!(name.as_str(), "td" | "th" | "template" | "caption");
        let void = is_void(&name);
        let skip_newline = matches!(name.as_str(), "pre" | "textarea");
        let allowed_in_table = matches!(
            name.as_str(),
            "caption"
                | "colgroup"
                | "col"
                | "tbody"
                | "thead"
                | "tfoot"
                | "tr"
                | "td"
                | "th"
                | "style"
                | "script"
                | "template"
        ) || (name == "input"
            && attributes
                .iter()
                .any(|(key, value)| key == "type" && value.eq_ignore_ascii_case("hidden")));
        let (parent, before) = if allowed_in_table {
            let parent = self.parent();
            (self.document.template_content(parent).ok().flatten().unwrap_or(parent), None)
        } else {
            self.insertion_location(self.parent())
        };
        let id = self
            .document
            .create(element(name, attributes))
            .map_err(|e| dom_error(start, e))?;
        self.document.insert_detached_before(parent, id, before);
        if !void {
            self.stack.push(id);
        }
        if formatting {
            let marker = self
                .formatting
                .iter()
                .rposition(Option::is_none)
                .map_or(0, |index| index + 1);
            let kind = self.document.kind(id).map_err(|e| dom_error(start, e))?;
            let mut duplicates =
                self.formatting[marker..]
                    .iter()
                    .enumerate()
                    .filter_map(|(index, &entry)| {
                        entry
                            .filter(|&entry| self.document.kind(entry).ok() == Some(kind))
                            .map(|_| marker + index)
                    });
            if let Some(first) = duplicates.next() {
                if duplicates.count() >= 2 {
                    self.formatting.remove(first);
                }
            }
            self.formatting.push(Some(id));
        }
        if marker {
            self.formatting.push(None);
        }
        if skip_newline && self.remaining().starts_with('\n') {
            self.pos += 1;
        }
        Ok(())
    }

    fn text(&mut self) -> Result<(), ParseError> {
        let start = self.pos;
        if self.remaining().starts_with('<') {
            self.pos += 1;
        }
        self.pos += self.remaining().find('<').unwrap_or(self.remaining().len());
        let whitespace = self.input[start..self.pos]
            .bytes()
            .all(|byte| byte.is_ascii_whitespace());
        if self.html.is_some() && !self.body_started && self.stack.is_empty() {
            if whitespace {
                return Ok(());
            }
            self.in_head = false;
            self.body_started = true;
        }
        if !whitespace
            || !matches!(self.document.kind(self.parent()), Ok(NodeKind::Element { name, .. }) if matches!(name.as_str(), "table" | "tbody" | "thead" | "tfoot" | "tr"))
        {
            self.reconstruct_formatting()?;
        }
        self.append_text(
            self.parent(),
            decode_entities(&self.input[start..self.pos], false),
            start,
        )?;
        Ok(())
    }

    fn name(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(&byte) = self.input.as_bytes().get(self.pos) {
            if !byte.is_ascii_whitespace() && !matches!(byte, b'/' | b'>') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(error(start, "expected name"));
        }
        Ok(self.input[start..self.pos].to_ascii_lowercase())
    }

    fn skip_space(&mut self) {
        while self
            .input
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }

    fn attribute_name(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        if self.input.as_bytes().get(self.pos) == Some(&b'=') {
            self.pos += 1;
        }
        while let Some(&byte) = self.input.as_bytes().get(self.pos) {
            if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>' | b'=') {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(error(start, "expected attribute name"));
        }
        Ok(self.input[start..self.pos].to_ascii_lowercase())
    }

    fn attribute_value(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        let quote = self.input.as_bytes().get(self.pos).copied();
        if matches!(quote, Some(b'\'' | b'"')) {
            self.pos += 1;
            let begin = self.pos;
            while self
                .input
                .as_bytes()
                .get(self.pos)
                .is_some_and(|&b| b != quote.unwrap())
            {
                self.pos += 1;
            }
            let value = decode_entities(&self.input[begin..self.pos], true);
            if self.pos < self.input.len() {
                self.pos += 1;
            }
            Ok(value)
        } else {
            while let Some(&byte) = self.input.as_bytes().get(self.pos) {
                if byte.is_ascii_whitespace() || byte == b'>' {
                    break;
                }
                self.pos += 1;
            }
            Ok(decode_entities(&self.input[start..self.pos], true))
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn template_contents_are_detached_cloned_and_reclaimed() {
        let mut doc = Document::new(40);
        let fragment = parse_fragment(&mut doc, "<template> <b>A</b><template><i>B</i></template></template>").unwrap();
        let template = doc.first_child(fragment).unwrap().unwrap();
        let content = doc.template_content(template).unwrap().unwrap();
        assert!(doc.first_child(template).unwrap().is_none());
        assert!(doc.parent(content).unwrap().is_none());
        assert_eq!(doc.append(content, template), Err(DomError::Hierarchy));
        assert_eq!(inner_html(&doc, template).unwrap(), " <b>A</b><template><i>B</i></template>");
        let clone = doc.clone_subtree(template).unwrap();
        assert_ne!(doc.template_content(clone).unwrap(), Some(content));
        assert_eq!(outer_html(&doc, clone).unwrap(), outer_html(&doc, template).unwrap());
        doc.destroy_subtree(clone).unwrap();
        doc.destroy_subtree(fragment).unwrap();
        assert_eq!(doc.node_count(), 1);
    }
    use super::*;

    #[test]
    fn formatting_recovery_reconstructs_and_adopts_blocks() {
        for (source, expected) in [
            (
                "<p>1<b>2<i>3</b>4</i>5</p>",
                "<p>1<b>2<i>3</i></b><i>4</i>5</p>",
            ),
            ("<b>1<p>2</b>3</p>", "<b>1</b><p><b>2</b>3</p>"),
            ("<p><b>A</p>B", "<p><b>A</b></p><b>B</b>"),
            ("<a>A<a>B</a>C", "<a>A</a><a>B</a>C"),
            (
                "<b><i><div>x</b>y</i>z",
                "<b><i></i></b><i></i><div><i><b>x</b>y</i>z</div>",
            ),
            (
                "<table><b><tr><td>aaa</td></tr>bbb</table>ccc",
                "<b></b><b>bbb</b><table><tbody><tr><td>aaa</td></tr></tbody></table><b>ccc</b>",
            ),
        ] {
            let mut doc = Document::new(64);
            let fragment = parse_fragment(&mut doc, source).unwrap();
            assert_eq!(inner_html(&doc, fragment).unwrap(), expected, "{source}");
            assert!(doc.mutations().is_empty());
        }
    }

    #[test]
    fn adoption_failure_reclaims_intermediate_clones() {
        let mut doc = Document::new(7);
        for _ in 0..4 {
            assert!(parse_fragment(&mut doc, "<b><i><div>x</b>").is_err());
            assert_eq!(doc.node_count(), 1);
        }
    }

    #[test]
    fn fragment_context_selects_table_and_text_tokenization() {
        for (name, source, expected) in [
            (
                "table",
                "<tr><td>A<td>B",
                "<tbody><tr><td>A</td><td>B</td></tr></tbody>",
            ),
            ("tbody", "<tr><td>A", "<tr><td>A</td></tr>"),
            ("tr", "<td>A<td>B", "<td>A</td><td>B</td>"),
            (
                "textarea",
                "<b>&amp;</textarea><p>x",
                "&lt;b&gt;&amp;&lt;/textarea&gt;&lt;p&gt;x",
            ),
            ("style", "<b>&amp;", "<b>&amp;"),
        ] {
            let mut doc = Document::new(32);
            let context = doc.create(element(name, Vec::new())).unwrap();
            let fragment = parse_fragment_in(&mut doc, context, source).unwrap();
            doc.append(context, fragment).unwrap();
            assert_eq!(inner_html(&doc, context).unwrap(), expected, "{name}");
        }
    }

    #[test]
    fn token_recovery_keeps_comments_and_discards_incomplete_tags() {
        for (source, expected) in [
            ("<div title=\"unfinished", ""),
            ("<p a=>A</p></p></br>", "<p a=\"\">A</p><p></p><br>"),
            (
                "<!bogus><!--unterminated",
                "<!--bogus--><!--unterminated-->",
            ),
            (
                "<select><option>A<option>B<optgroup label=x><option>C</select>",
                "<select><option>A</option><option>B</option><optgroup label=\"x\"><option>C</option></optgroup></select>",
            ),
            (
                "<template><tr><td>A</template>B",
                "<template><tr><td>A</td></tr></template>B",
            ),
            (
                "<table><tr><td><b>A<td>B</table>C",
                "<table><tbody><tr><td><b>A</b></td><td>B</td></tr></tbody></table>C",
            ),
        ] {
            let mut doc = Document::new(32);
            let fragment = parse_fragment(&mut doc, source).unwrap();
            assert_eq!(inner_html(&doc, fragment).unwrap(), expected, "{source}");
        }
    }

    #[test]
    fn parses_document_tree() {
        let doc = parse("<!doctype html><html><head><style>p{color:red}</style></head><body><p id='x'>hi</p></body></html>", 16).unwrap();
        let root = doc.root();
        let doctype = doc.first_child(root).unwrap().unwrap();
        assert_eq!(
            doc.kind(doctype).unwrap(),
            &NodeKind::DocumentType("html".to_string())
        );
        let html = doc.next_sibling(doctype).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let style = doc.first_child(head).unwrap().unwrap();
        assert!(
            matches!(doc.kind(style).unwrap(), NodeKind::Element { name, .. } if name == "style")
        );
    }

    #[test]
    fn style_and_script_keep_raw_text_inert() {
        let doc = parse(
            "<style>p::before{content:'<';}</style><script>if (a < b) run()</script>",
            12,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let style = doc.first_child(head).unwrap().unwrap();
        let text = doc.first_child(style).unwrap().unwrap();
        assert!(matches!(doc.kind(text).unwrap(), NodeKind::Text(value) if value.contains("'<'")));
        let script = doc.next_sibling(style).unwrap().unwrap();
        assert!(
            matches!(doc.kind(script).unwrap(), NodeKind::Element { name, .. } if name == "script")
        );
    }

    #[test]
    fn raw_and_rcdata_require_an_appropriate_end_tag() {
        let mut doc = Document::new(24);
        let fragment = parse_fragment(&mut doc,
            "<script>a</scriptx>&amp;<b></SCRIPT><textarea>\nA &amp; <b></textareax> B</textarea><title>&lt;x&gt;</title>").unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<script>a</scriptx>&amp;<b></script><textarea>A &amp; &lt;b&gt;&lt;/textareax&gt; B</textarea><title>&lt;x&gt;</title>"
        );
    }

    #[test]
    fn script_double_escaped_end_tag_remains_inert_text() {
        let source = "<script><!--<script>var a = 1;</script>after</script><p>x";
        let mut doc = Document::new(16);
        let fragment = parse_fragment(&mut doc, source).unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<script><!--<script>var a = 1;</script>after</script><p>x</p>"
        );
    }

    #[test]
    fn tokenizer_normalizes_newlines_nulls_and_attribute_names() {
        let mut doc = Document::new(12);
        let fragment = parse_fragment(
            &mut doc,
            "<pre>\r\na\rb\r\nc\0</pre><p data.foo='x' @click='y' café='z'>",
        )
        .unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "<pre>a\nb\nc\u{fffd}</pre><p data.foo=\"x\" @click=\"y\" café=\"z\"></p>"
        );
        assert!(matches!(normalized_input("unchanged"), Cow::Borrowed(_)));
    }

    #[test]
    fn decodes_text_and_attribute_references() {
        let doc = parse("<p title='A &amp; B'>&lt;ok&#x21;&unknown;</p>", 6).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let body = doc
            .next_sibling(doc.first_child(html).unwrap().unwrap())
            .unwrap()
            .unwrap();
        let p = doc.first_child(body).unwrap().unwrap();
        assert!(
            matches!(doc.kind(p).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "A & B")
        );
        let text = doc.first_child(p).unwrap().unwrap();
        assert_eq!(
            doc.kind(text).unwrap(),
            &NodeKind::Text("<ok!&unknown;".to_string())
        );
        assert_eq!(
            outer_html(&doc, p).unwrap(),
            "<p title=\"A &amp; B\">&lt;ok!&amp;unknown;</p>"
        );
    }

    #[test]
    fn whatwg_named_and_numeric_references() {
        assert_eq!(
            decode_entities("&copy and &acE; &#x80; &#0; &unknown;", false),
            "\u{a9} and \u{223e}\u{333} \u{20ac} \u{fffd} &unknown;"
        );
        assert_eq!(
            decode_entities("x&copy=1 &copy;!", true),
            "x&copy=1 \u{a9}!"
        );
    }

    #[test]
    fn fragment_parses_in_arena_and_rolls_back_errors() {
        let mut doc = Document::new(16);
        let fragment = parse_fragment(&mut doc, "<b>A</b><i>B</i>").unwrap();
        assert_eq!(inner_html(&doc, fragment).unwrap(), "<b>A</b><i>B</i>");
        assert!(doc.drain_mutations().is_empty());
        let copy = doc.clone_subtree(fragment).unwrap();
        assert_eq!(inner_html(&doc, copy).unwrap(), "<b>A</b><i>B</i>");
        let count = doc.node_count();
        assert!(parse_fragment(&mut doc, "<font>oops").is_err());
        assert_eq!(doc.node_count(), count);
        let allocated = doc.nodes.len();
        for _ in 0..8 {
            assert!(parse_fragment(&mut doc, "<font>oops").is_err());
            assert_eq!(doc.node_count(), count);
            assert_eq!(doc.nodes.len(), allocated);
        }
    }

    #[test]
    fn document_scaffold_keeps_explicit_attributes() {
        let doc = parse(
            "<html lang='en'><head id='h'></head><body style='background:red'></body></html>",
            4,
        )
        .unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert!(
            matches!(doc.kind(html).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "en")
        );
        assert!(
            matches!(doc.kind(head).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "h")
        );
        assert!(
            matches!(doc.kind(body).unwrap(), NodeKind::Element { attributes, .. } if attributes[0].1 == "background:red")
        );
    }

    #[test]
    fn implied_end_tags_and_adjacent_text_match_html_tree() {
        let doc = parse("<p>one<p>two<div>three</div><ul><li>A<li>B</ul>x < y", 24).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<p>one</p><p>two</p><div>three</div><ul><li>A</li><li>B</li></ul>x &lt; y"
        );
    }

    #[test]
    fn nested_lists_keep_outer_item_open() {
        let doc = parse("<ul><li>outer<ul><li>inner</li></ul><li>next</ul>", 20).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<ul><li>outer<ul><li>inner</li></ul></li><li>next</li></ul>"
        );
    }

    #[test]
    fn table_rows_insert_tbody_and_close_cells() {
        let doc = parse("<table><tr><td>A<td>B<tr><td>C</table>", 24).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(
            inner_html(&doc, body).unwrap(),
            "<table><tbody><tr><td>A</td><td>B</td></tr><tr><td>C</td></tr></tbody></table>"
        );
    }

    #[test]
    fn table_recovery_fosters_content_and_implies_missing_containers() {
        let mut doc = Document::new(32);
        let fragment =
            parse_fragment(&mut doc, "<table>before<div>x</div><td>A<td>B</table>after").unwrap();
        assert_eq!(
            inner_html(&doc, fragment).unwrap(),
            "before<div>x</div><table><tbody><tr><td>A</td><td>B</td></tr></tbody></table>after"
        );
        let columns = parse_fragment(&mut doc, "<table><col><tr><td>C</table>").unwrap();
        assert_eq!(
            inner_html(&doc, columns).unwrap(),
            "<table><colgroup><col></colgroup><tbody><tr><td>C</td></tr></tbody></table>"
        );
        assert!(doc.mutations().is_empty());
    }

    #[test]
    fn rejects_obsolete_elements_in_standards_profile() {
        assert_eq!(
            parse("<font>legacy</font>", 8).err().unwrap().message,
            "obsolete element"
        );
        assert_eq!(
            parse("<svg><path></path></svg>", 8).err().unwrap().message,
            "foreign content is unsupported"
        );
    }

    #[test]
    fn self_closing_tag_keeps_unquoted_attribute_value() {
        let doc = parse("<img src=tile.png/>", 8).unwrap();
        let html = doc.first_child(doc.root()).unwrap().unwrap();
        let head = doc.first_child(html).unwrap().unwrap();
        let body = doc.next_sibling(head).unwrap().unwrap();
        assert_eq!(inner_html(&doc, body).unwrap(), "<img src=\"tile.png/\">");
        let mut fragment_doc = Document::new(8);
        let fragment = parse_fragment(&mut fragment_doc, "<div/>text").unwrap();
        assert_eq!(
            inner_html(&fragment_doc, fragment).unwrap(),
            "<div>text</div>"
        );
    }
}
