//! Argument grammar of the `::view-transition-*` functional pseudo-elements:
//! `[ '*' | <custom-ident> ] [ '.' <custom-ident> ]*`.
use super::*;

pub(super) struct ViewTransitionArgument {
    pub wildcard: bool,
    pub name: String,
    pub classes: Vec<String>,
}

fn reserved_ident(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "none" | "initial" | "inherit" | "unset" | "revert" | "revert-layer" | "default"
    )
}

pub(super) fn parse_argument(input: &str) -> Option<ViewTransitionArgument> {
    let input = input.trim_matches(is_css_whitespace_char);
    let mut position = 0;
    let (wildcard, name) = if input.starts_with('*') {
        position = 1;
        (true, String::from("*"))
    } else {
        let name = consume_selector_identifier(input, &mut position)?;
        if reserved_ident(&name) {
            return None;
        }
        (false, name)
    };
    let mut classes = Vec::new();
    while position < input.len() {
        if input.as_bytes()[position] != b'.' {
            return None;
        }
        position += 1;
        let class = consume_selector_identifier(input, &mut position)?;
        if reserved_ident(&class) {
            return None;
        }
        classes.push(class);
    }
    Some(ViewTransitionArgument { wildcard, name, classes })
}

impl ViewTransitionArgument {
    pub(super) fn serialize(&self) -> String {
        let mut output = if self.wildcard { String::from("*") } else { serialize_identifier(&self.name) };
        for class in &self.classes {
            output.push('.');
            output.push_str(&serialize_identifier(class));
        }
        output
    }

    /// Lookup key distinguishing the wildcard from an identifier spelled `*`
    /// and carrying the class filter; plain names stay verbatim.
    pub(super) fn key(&self) -> String {
        if self.wildcard && self.classes.is_empty() {
            return String::from("*");
        }
        self.serialize()
    }
}

fn is_css_whitespace_char(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\n' | '\r' | '\u{c}')
}

/// A `getComputedStyle()` pseudo-element argument naming one generated
/// view-transition pseudo: the pseudo and, when functional, its capture name.
/// Wildcard and class-filtered arguments name no single pseudo-element.
pub fn parse_computed_target(input: &str) -> Option<(PseudoElement, Option<String>)> {
    let rest = input.strip_prefix("::")?;
    let (name, argument) = match rest.find('(') {
        Some(open) => {
            let argument = rest[open + 1..].strip_suffix(')')?;
            (&rest[..open], Some(argument))
        }
        None => (rest, None),
    };
    let pseudo = match name.to_ascii_lowercase().as_str() {
        "view-transition" => return argument.is_none().then_some((PseudoElement::ViewTransition, None)),
        "view-transition-group" => PseudoElement::ViewTransitionGroup,
        "view-transition-image-pair" => PseudoElement::ViewTransitionImagePair,
        "view-transition-old" => PseudoElement::ViewTransitionOld,
        "view-transition-new" => PseudoElement::ViewTransitionNew,
        _ => return None,
    };
    let argument = parse_argument(argument?)?;
    if argument.wildcard || !argument.classes.is_empty() {
        return None;
    }
    Some((pseudo, Some(argument.name)))
}
