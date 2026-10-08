use std::borrow::Cow;
use super::{scan, parser, is_xml_char};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PseudoAttribute<'a> {
    pub name: &'a str,
    pub value: Cow<'a, str>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PseudoAttributeError { Invalid, LimitExceeded }

/// XML stylesheet pseudo-attributes use XML Name/reference scanning, but do
/// not use attribute-value whitespace normalization or DTD entity expansion.
pub fn parse_pseudo_attributes(input: &str, max_bytes: usize, max_attributes: usize)
    -> Result<Vec<PseudoAttribute<'_>>, PseudoAttributeError>
{
    use PseudoAttributeError::{Invalid, LimitExceeded};
    if input.len() > max_bytes { return Err(LimitExceeded); }
    let mut attributes = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut cursor = scan::skip_space(input, 0);
    while cursor < input.len() {
        if attributes.len() == max_attributes { return Err(LimitExceeded); }
        let end = scan::name_end(input, cursor).ok_or(Invalid)?;
        let name = &input[cursor..end];
        let position = names.binary_search(&name).err().ok_or(Invalid)?;
        names.try_reserve(1).map_err(|_| LimitExceeded)?;
        names.insert(position, name);
        cursor = scan::skip_space(input, end);
        if input.as_bytes().get(cursor) != Some(&b'=') { return Err(Invalid); }
        cursor = scan::skip_space(input, cursor + 1);
        let quote = *input.as_bytes().get(cursor).filter(|byte| **byte == b'\'' || **byte == b'"').ok_or(Invalid)?;
        cursor += 1;
        let start = cursor;
        let mut decoded: Option<String> = None;
        let mut copy_start = start;
        loop {
            let ch = input.get(cursor..).and_then(|tail| tail.chars().next()).ok_or(Invalid)?;
            if ch as u32 == quote as u32 { break; }
            if ch == '<' || !is_xml_char(ch) { return Err(Invalid); }
            if ch == '&' {
                let (value, next) = match scan::reference(input, cursor) {
                    scan::Scan::Tok(scan::Ref::Char(Some(value)), next) => (value, next),
                    scan::Scan::Tok(scan::Ref::Entity(name), next) => (parser::predefined(name).ok_or(Invalid)?, next),
                    _ => return Err(Invalid),
                };
                let output = decoded.get_or_insert_with(String::new);
                output.try_reserve(cursor - copy_start + value.len_utf8()).map_err(|_| LimitExceeded)?;
                output.push_str(&input[copy_start..cursor]); output.push(value);
                cursor = next; copy_start = next;
            } else { cursor += ch.len_utf8(); }
        }
        let value = match decoded {
            None => Cow::Borrowed(&input[start..cursor]),
            Some(mut output) => {
                output.try_reserve(cursor - copy_start).map_err(|_| LimitExceeded)?;
                output.push_str(&input[copy_start..cursor]); Cow::Owned(output)
            }
        };
        attributes.try_reserve(1).map_err(|_| LimitExceeded)?;
        attributes.push(PseudoAttribute { name, value });
        cursor += 1;
        let next = scan::skip_space(input, cursor);
        if next == cursor && cursor < input.len() { return Err(Invalid); }
        cursor = next;
    }
    Ok(attributes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_xml_stylesheet_pseudo_attributes_preserve_xml_units_and_reject_duplicates() {
        let attrs = parse_pseudo_attributes("href='a&amp;b' title=\"A\tB\nC&#13;&#x1F600;\" X='one' x='two'", 4096, 32).unwrap();
        assert_eq!(attrs[0].value, "a&b");
        assert_eq!(attrs[1].value, "A\tB\nC\r😀");
        assert!(matches!(attrs[2].value, Cow::Borrowed(_)));
        for input in ["x='1' x='2'", "href='&external;'", "href='&#0;'", "href='&#xD800;'", "href='a<b'", "x='1'y='2'", "href=x", "href='&amp'"] {
            assert_eq!(parse_pseudo_attributes(input, 4096, 32), Err(PseudoAttributeError::Invalid), "{input}");
        }
        assert_eq!(parse_pseudo_attributes("x='1'", 4, 32), Err(PseudoAttributeError::LimitExceeded));
        assert_eq!(parse_pseudo_attributes("x='1' y='2'", 4096, 1), Err(PseudoAttributeError::LimitExceeded));
        assert!(parse_pseudo_attributes(" \n\t", 4096, 1).unwrap().is_empty());
    }
}
