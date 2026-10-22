//! Media query list parsing and canonical serialization (Media Queries 4 §3,
//! CSSOM §6.1). An invalid query is replaced by `not all`; unknown features
//! stay valid as general-enclosed components and evaluate to unknown.
use super::*;
use syntax::{Cursor, TokenKind};

const MAX_DEPTH: usize = 32;

enum Node<'a> {
    Space,
    Token(&'a str),
    Block { open: u8, function: Option<&'a str>, children: Vec<Node<'a>> },
}

struct Frame<'a> {
    open: u8,
    function: Option<&'a str>,
    children: Vec<Node<'a>>,
}

fn is_identifier_start(token: &str) -> bool {
    let mut position = 0;
    consume_selector_identifier(token, &mut position).is_some_and(|_| position == token.len())
}

fn push_space(nodes: &mut Vec<Node<'_>>) {
    if !matches!(nodes.last(), Some(Node::Space)) {
        nodes.push(Node::Space);
    }
}

fn segments(input: &str) -> Option<Vec<Option<Vec<Node<'_>>>>> {
    let mut cursor = Cursor::new(input, 0).ok()?;
    let mut result = Vec::new();
    let mut root: Vec<Node<'_>> = Vec::new();
    let mut stack: Vec<Frame<'_>> = Vec::new();
    let mut bad = false;
    while let Some(token) = cursor.next() {
        let text = &input[token.start..token.end];
        match token.kind {
            TokenKind::Comment { .. } => {
                push_space(stack.last_mut().map_or(&mut root, |frame| &mut frame.children));
            }
            TokenKind::Other if matches!(text, " " | "\t" | "\n" | "\r" | "\x0c") => {
                push_space(stack.last_mut().map_or(&mut root, |frame| &mut frame.children));
            }
            TokenKind::Other if text == "," && stack.is_empty() => {
                result.push((!bad).then(|| core::mem::take(&mut root)));
                root.clear();
                bad = false;
            }
            TokenKind::Open(open) => {
                if stack.len() >= MAX_DEPTH {
                    bad = true;
                    continue;
                }
                let container = stack.last_mut().map_or(&mut root, |frame| &mut frame.children);
                let mut function = None;
                if open == b'(' {
                    if let Some(Node::Token(name)) = container.last() {
                        if is_identifier_start(name) {
                            function = Some(*name);
                        }
                    }
                    if function.is_some() {
                        container.pop();
                    }
                }
                stack.push(Frame { open, function, children: Vec::new() });
            }
            TokenKind::Close(_) => match stack.pop() {
                Some(frame) => {
                    let block = Node::Block { open: frame.open, function: frame.function, children: frame.children };
                    stack.last_mut().map_or(&mut root, |parent| &mut parent.children).push(block);
                }
                None => bad = true,
            },
            TokenKind::BadString | TokenKind::BadUrl => bad = true,
            _ => stack.last_mut().map_or(&mut root, |frame| &mut frame.children).push(Node::Token(text)),
        }
    }
    while let Some(frame) = stack.pop() {
        let block = Node::Block { open: frame.open, function: frame.function, children: frame.children };
        stack.last_mut().map_or(&mut root, |parent| &mut parent.children).push(block);
    }
    result.push((!bad).then_some(root));
    Some(result)
}

fn solid<'b, 'a>(nodes: &'b [Node<'a>]) -> Vec<&'b Node<'a>> {
    nodes.iter().filter(|node| !matches!(node, Node::Space)).collect()
}

fn keyword(node: &Node<'_>) -> Option<String> {
    let Node::Token(text) = node else { return None };
    let mut position = 0;
    let name = consume_selector_identifier(text, &mut position)?;
    (position == text.len()).then(|| name.to_ascii_lowercase())
}

fn is_operator(text: &str) -> bool {
    matches!(text, "<" | ">" | "=")
}

fn serialize_inner(nodes: &[Node<'_>], top: bool) -> String {
    let mut output = String::new();
    let mut pending = false;
    let mut index = 0;
    while index < nodes.len() {
        match &nodes[index] {
            Node::Space => pending = true,
            Node::Token(text) if *text == ":" && top => {
                output.push(':');
                pending = true;
            }
            Node::Token(text) if top && is_operator(text) => {
                let mut operator = String::from(*text);
                while let Some(Node::Token(next)) = nodes.get(index + 1) {
                    if *next == "=" && !operator.ends_with('=') && operator != "=" {
                        operator.push('=');
                        index += 1;
                    } else {
                        break;
                    }
                }
                if !output.is_empty() {
                    output.push(' ');
                }
                output.push_str(&operator);
                pending = true;
            }
            Node::Token(text) => {
                if pending && !output.is_empty() {
                    output.push(' ');
                }
                pending = false;
                if top && is_identifier_start(text) {
                    output.push_str(&text.to_ascii_lowercase());
                } else {
                    output.push_str(text);
                }
            }
            Node::Block { open, function, children } => {
                if pending && !output.is_empty() {
                    output.push(' ');
                }
                pending = false;
                if let Some(name) = function {
                    output.push_str(&name.to_ascii_lowercase());
                }
                let (start, end) = match open {
                    b'(' => ('(', ')'),
                    b'[' => ('[', ']'),
                    _ => ('{', '}'),
                };
                output.push(start);
                output.push_str(serialize_inner(children, false).trim());
                output.push(end);
            }
        }
        index += 1;
    }
    output
}

fn in_parens(node: &Node<'_>, depth: usize) -> Option<String> {
    let Node::Block { open: b'(', function, children } = node else { return None };
    if let Some(name) = function {
        return Some(alloc::format!("{}({})", name.to_ascii_lowercase(), serialize_inner(children, false).trim()));
    }
    let inner = solid(children);
    if inner.is_empty() {
        return None;
    }
    if let Some(condition) = condition(&inner, true, depth + 1) {
        return Some(alloc::format!("({condition})"));
    }
    Some(alloc::format!("({})", serialize_inner(children, true).trim()))
}

fn condition(nodes: &[&Node<'_>], allow_or: bool, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH || nodes.is_empty() {
        return None;
    }
    if keyword(nodes[0]).as_deref() == Some("not") {
        return (nodes.len() == 2).then(|| in_parens(nodes[1], depth)).flatten().map(|inner| alloc::format!("not {inner}"));
    }
    let first = in_parens(nodes[0], depth)?;
    if nodes.len() == 1 {
        return Some(first);
    }
    if nodes.len() % 2 == 0 {
        return None;
    }
    let connective = keyword(nodes[1])?;
    if connective != "and" && !(connective == "or" && allow_or) {
        return None;
    }
    let mut output = first;
    for pair in nodes[1..].chunks_exact(2) {
        if keyword(pair[0]).as_deref() != Some(connective.as_str()) {
            return None;
        }
        output.push(' ');
        output.push_str(&connective);
        output.push(' ');
        output.push_str(&in_parens(pair[1], depth)?);
    }
    Some(output)
}

fn query(nodes: &[Node<'_>]) -> Option<String> {
    let nodes = solid(nodes);
    if nodes.is_empty() {
        return None;
    }
    if let Some(condition) = condition(&nodes, true, 0) {
        return Some(condition);
    }
    let mut output = String::new();
    let mut index = 0;
    let first = keyword(nodes[0])?;
    if first == "not" || first == "only" {
        output.push_str(&first);
        output.push(' ');
        index = 1;
    }
    let media_type = keyword(nodes.get(index)?)?;
    if matches!(media_type.as_str(), "not" | "only" | "and" | "or" | "layer") {
        return None;
    }
    let rest = &nodes[index + 1..];
    if index == 0 && media_type == "all" && !rest.is_empty() {
        if keyword(rest[0]).as_deref() != Some("and") {
            return None;
        }
        return condition(&rest[1..], false, 0);
    }
    output.push_str(&serialize_identifier(&media_type));
    if !rest.is_empty() {
        if keyword(rest[0]).as_deref() != Some("and") {
            return None;
        }
        output.push_str(" and ");
        output.push_str(&condition(&rest[1..], false, 0)?);
    }
    Some(output)
}

/// The serialized queries of a media query list. Blank input is the empty list.
pub fn serialize_list_items(input: &str) -> Vec<String> {
    if input.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')).is_empty() {
        return Vec::new();
    }
    let Some(parsed) = segments(input) else {
        return alloc::vec![String::from("not all")];
    };
    parsed
        .into_iter()
        .map(|segment| segment.and_then(|nodes| query(&nodes)).unwrap_or_else(|| String::from("not all")))
        .collect()
}

/// A media query list in canonical serialized form.
pub fn serialize_list(input: &str) -> String {
    serialize_list_items(input).join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_and_replaces_invalid_queries() {
        for (source, expected) in [
            ("", ""),
            (" all , all ", "all, all"),
            ("(color", "(color)"),
            (" ( color  )  ", "(color)"),
            ("color)", "not all"),
            ("  color ), ( color", "not all, (color)"),
            (",", "not all, not all"),
            (" foo,", "foo, not all"),
            ("(min-width:0px)", "(min-width: 0px)"),
            ("(400px<=width<=700px)", "(400px <= width <= 700px)"),
            ("only (orientation)", "not all"),
            ("not not (width)", "not all"),
            ("SCREEN AND (Color)", "screen and (color)"),
            ("screen or (width)", "not all"),
            ("not unknown(width) ", "not unknown(width)"),
            ("all and color :", "not all"),
            ("(((((width", "(((((width)))))"),
            ("and", "not all"),
        ] {
            assert_eq!(serialize_list(source), expected, "{source}");
        }
    }
}
