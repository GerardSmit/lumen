//! CSSOM selector serialization over canonical token readers.
//! Namespace context is operation-local; owners retain canonical rule text.
use super::*;
use alloc::format;

fn push(output: &mut String, value: &str) -> Result<(), CssError> {
    output.len().checked_add(value.len()).filter(|length| *length <= MAX_CSS_BYTES)
        .ok_or_else(|| selector_error(0, "serialized selector exceeds limit"))?;
    output.try_reserve(value.len()).map_err(|_| selector_error(0, "selector serialization allocation failed"))?;
    output.push_str(value); Ok(())
}
pub fn serialize(input: &str, map: &NamespaceMap, nesting: bool) -> Result<String, CssError> {
    list(input, map, nesting, false, false, false, 0)
}
fn list(input: &str, map: &NamespaceMap, nesting: bool, relative: bool, forgiving: bool, logical_subject: bool, depth: usize) -> Result<String, CssError> {
    if depth > MAX_SELECTOR_NESTING { return Err(selector_error(0, "selector nesting limit exceeded")); }
    let completed = syntax::complete(input)?.ok_or_else(|| selector_error(0, "invalid selector component"))?;
    let clean = completed.contains("/*").then(|| strip_selector_comments(&completed, 0)).transpose()?;
    let input = clean.as_deref().unwrap_or(&completed);
    let mut output = String::new();
    for (start, end) in selector_list_spans(input, 0, forgiving)? {
        let part = &input[start..end];
        let valid = if relative { parse_relative_selector_list_depth(part, start, depth, nesting, map).map(|_| ()) }
            else { parse_selector_depth(part, start, depth, nesting, map).map(|_| ()) };
        if let Err(error) = valid { if forgiving && !selector_limit_error(&error) { continue; } return Err(error); }
        let value = chain(part, map, nesting, relative, logical_subject, depth)?;
        if !output.is_empty() { push(&mut output, ", ")?; } push(&mut output, &value)?;
    }
    Ok(output)
}
fn chain(input: &str, map: &NamespaceMap, nesting: bool, relative: bool, logical_subject: bool, depth: usize) -> Result<String, CssError> {
    let input = trim_selector_source(input); let mut output = String::new(); let mut position = 0;
    if relative && input.as_bytes().first().is_some_and(|byte| matches!(byte, b'>' | b'+' | b'~')) {
        push(&mut output, &input[..1])?; push(&mut output, " ")?; position = 1;
    }
    while position < input.len() {
        while input.as_bytes().get(position).is_some_and(u8::is_ascii_whitespace) { position += 1; }
        let start = position; let end = selector_compound_end(input, position, 0)?;
        let mut next = end; while input.as_bytes().get(next).is_some_and(u8::is_ascii_whitespace) { next += 1; }
        push(&mut output, &compound(&input[start..end], map, nesting, logical_subject && next == input.len(), depth)?)?;
        if next == input.len() { break; }
        if matches!(input.as_bytes()[next], b'>' | b'+' | b'~') {
            push(&mut output, " ")?; push(&mut output, &input[next..next + 1])?; push(&mut output, " ")?; next += 1;
        } else { push(&mut output, " ")?; }
        position = next;
    }
    Ok(output)
}
fn compound(input: &str, map: &NamespaceMap, nesting: bool, logical_subject: bool, depth: usize) -> Result<String, CssError> {
    let parsed = parse_selector_type(input, 0, map)?;
    let default = map.resolve(None).map_or(NamespaceConstraint::Any, NamespaceConstraint::Uri);
    let omit = parsed.universal && parsed.end < input.len() && !(logical_subject && map.resolve(None).is_some()) && parsed.namespace == default;
    let mut output = String::new();
    if parsed.explicit_type && !omit {
        match &parsed.namespace {
            NamespaceConstraint::Any => if map.resolve(None).is_some() { push(&mut output, "*|")?; },
            NamespaceConstraint::None => push(&mut output, "|")?,
            NamespaceConstraint::Uri(uri) if uri.is_empty() => push(&mut output, "|")?,
            NamespaceConstraint::Uri(_) if parsed.namespace != default => {
                push(&mut output, &serialize_identifier(parsed.prefix.as_deref().ok_or_else(|| selector_error(0, "missing namespace prefix"))?))?;
                push(&mut output, "|")?;
            },
            NamespaceConstraint::Uri(_) => {},
        }
        if parsed.universal { push(&mut output, "*")?; }
        else if let Some(tag) = &parsed.tag { push(&mut output, &serialize_identifier(tag))?; }
    }
    let mut position = parsed.end;
    while position < input.len() {
        match input.as_bytes()[position] {
            b'.' | b'#' => {
                push(&mut output, &input[position..position + 1])?; position += 1;
                let name = consume_selector_identifier(input, &mut position).ok_or_else(|| selector_error(position, "invalid selector identifier"))?;
                push(&mut output, &serialize_identifier(&name))?;
            },
            b'[' => {
                let end = matching_css_block(input, position).ok_or_else(|| selector_error(position, "unterminated attribute selector"))?;
                push(&mut output, &attribute(&input[position + 1..end - 1], map)?)?; position = end;
            },
            b':' => {
                let rest = &input[position..]; let pseudo = parse_pseudo_source(rest, position)?;
                let element = rest.starts_with("::") || matches!(pseudo.name.as_str(), "before" | "after" | "first-line" | "first-letter");
                push(&mut output, if element { "::" } else { ":" })?; push(&mut output, &serialize_identifier(&pseudo.name))?;
                if let Some(args) = pseudo.args {
                    push(&mut output, "(")?; push(&mut output, &arguments(&pseudo.name, args, map, nesting, depth + 1)?)?; push(&mut output, ")")?;
                }
                position += pseudo.end;
            },
            b'&' => { push(&mut output, "&")?; position += 1; },
            _ => return Err(selector_error(position, "invalid selector component")),
        }
    }
    Ok(output)
}
fn attribute(input: &str, map: &NamespaceMap) -> Result<String, CssError> {
    let parsed = parse_attribute_selector(input, 0, map)?; let mut output = String::from("[");
    match &parsed.namespace {
        AttributeNamespace::Any => push(&mut output, "*|")?,
        AttributeNamespace::Uri(uri) if !uri.is_empty() => {
            let input = input.trim_ascii_start(); let mut position = 0;
            let prefix = consume_selector_identifier(input, &mut position).ok_or_else(|| selector_error(0, "missing attribute namespace prefix"))?;
            push(&mut output, &serialize_identifier(&prefix))?; push(&mut output, "|")?;
        },
        _ => {},
    }
    push(&mut output, &serialize_identifier(&parsed.name))?;
    push(&mut output, match parsed.operator {
        AttributeOperator::Exists => "", AttributeOperator::Equals => "=", AttributeOperator::Includes => "~=",
        AttributeOperator::DashMatch => "|=", AttributeOperator::Prefix => "^=", AttributeOperator::Suffix => "$=", AttributeOperator::Substring => "*=",
    })?;
    if let Some(value) = parsed.value { push(&mut output, &serialize_string(&value))?; }
    match parsed.case_sensitivity {
        AttributeCaseSensitivity::Default => {}, AttributeCaseSensitivity::AsciiInsensitive => push(&mut output, " i")?,
        AttributeCaseSensitivity::Sensitive => push(&mut output, " s")?,
    }
    push(&mut output, "]")?; Ok(output)
}
fn arguments(name: &str, input: &str, map: &NamespaceMap, nesting: bool, depth: usize) -> Result<String, CssError> {
    match name {
        "is" | "where" | "not" => list(input, map, nesting, false, name != "not", true, depth),
        "has" => list(input, map, nesting, true, false, false, depth),
        "host" => list(input, map, nesting, false, false, true, depth),
        "slotted" => list(input, map, nesting, false, false, false, depth),
        "nth-child" | "nth-last-child" | "nth-of-type" | "nth-last-of-type" => {
            let ((a, b), _) = parse_nth_argument(input, 0, depth, matches!(name, "nth-child" | "nth-last-child"), nesting, map)?;
            let mut output = if a == 0 { b.to_string() } else {
                let mut result = match a { 1 => String::from("n"), -1 => String::from("-n"), _ => format!("{a}n") };
                if b > 0 { result.push('+'); result.push_str(&b.to_string()); } else if b < 0 { result.push_str(&b.to_string()); } result
            };
            if let Some((_, end)) = nth_of_separator(input, 0)? {
                push(&mut output, " of ")?; push(&mut output, &list(&input[end..], map, nesting, false, false, false, depth)?)?;
            } Ok(output)
        },
        "lang" => {
            let values = language_argument_values(input).ok_or_else(|| selector_error(0, "invalid language range"))?;
            Ok(values.iter().map(|value| serialize_string(value)).collect::<Vec<_>>().join(", "))
        },
        "dir" => Ok(if directionality_argument(input,0)?==1{String::from("ltr")}else{String::from("rtl")}),
        "state" => Ok(serialize_identifier(&custom_state_argument(input).ok_or_else(|| selector_error(0, "invalid state identifier"))?)),
        "highlight" => {
            let input = input.trim(); let mut position = 0;
            let name = consume_selector_identifier(input, &mut position).filter(|_| position == input.len()).ok_or_else(|| selector_error(0, "invalid highlight identifier"))?;
            Ok(serialize_identifier(&name))
        },
        "part" => Ok(input.split_ascii_whitespace().map(serialize_identifier).collect::<Vec<_>>().join(" ")),
        "heading" => {
            let values = comma_components(input, MAX_SELECTOR_COMPOUNDS).ok_or_else(|| selector_error(0, "invalid heading list"))?;
            Ok(values.iter().map(|value| {
                let value=value.trim();let negative=value.starts_with('-');
                let digits=value.trim_start_matches(['+','-']).trim_start_matches('0');
                if digits.is_empty(){String::from("0")}else if negative{format!("-{digits}")}else{String::from(digits)}
            }).collect::<Vec<_>>().join(", "))
        },
        _ => Err(selector_error(0, "unsupported functional selector")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_cssom_selector_serialization_namespaces_and_universals() {
        let empty=NamespaceMap::default();
        let map=NamespaceMap::from_stylesheet("@namespace url('urn:default');@namespace same url('urn:default');@namespace other url('urn:other');@namespace ns\\:odd url('urn:odd');").unwrap();
        for (source,expected) in [("*.class",".class"),("*|*.class","*|*.class"),("same|*.class",".class"),("other|*.class","other|*.class"),("|*.class","|*.class"),("same|e > other|e","e > other|e"),(r"ns\:odd|odd\:name",r"ns\:odd|odd\:name"),(r"[ns\:odd|odd\:name = 'value' I]",r#"[ns\:odd|odd\:name="value" i]"#)] {
            assert_eq!(serialize(source,&map,false).unwrap(),expected,"{source}");
        }
        assert_eq!(serialize("*|*.class, *|e, [|attr]",&empty,false).unwrap(),".class, e, [attr]");
        assert!(serialize("missing|e",&map,false).is_err());
    }
    #[test]
    fn specification_cssom_selector_serialization_logical_subjects_and_functions() {
        let map=NamespaceMap::from_stylesheet("@namespace url('urn:default');@namespace other url('urn:other');").unwrap();
        let cases=[
            (":is( *.class, .class, missing|e)",":is(*.class, .class)"),
            (":not(*.class)",":not(*.class)"),
            (":where(other|*.class)",":where(other|*.class)"),
            (":host(*.class)",":host(*.class)"),
            ("::slotted(*.class)","::slotted(.class)"),
            (":has(> *.class + other|e)",":has(> .class + other|e)"),
            (":nth-child(odd of *.class,other|e)",":nth-child(2n+1 of .class, other|e)"),
            (":nth-last-of-type(-1n - 0)",":nth-last-of-type(-n)"),
            (r":l\61 ng(EN-us, 'a b')",":lang(\"EN-us\", \"a b\")"),
            (r":d\69 r( r\74 l )",":dir(rtl)"),
            (":before","::before"),
        ];
        for (source,expected) in cases {assert_eq!(serialize(source,&map,false).unwrap(),expected,"{source}");}
        assert!(serialize(":not(missing|e)",&map,false).is_err());
        assert_eq!(serialize(r".ending\ ",&map,false).unwrap(),r".ending\ ");
        assert_eq!(serialize("& > *.class",&map,true).unwrap(),"& > .class");
        assert!(serialize("& > .class",&map,false).is_err());
        assert!(serialize(":marker",&map,false).is_err());
        assert!(serialize("::lang(en)",&map,false).is_err());
        assert_eq!(serialize("::marker",&map,false).unwrap(),"::marker");
    }
    #[test]
    fn specification_cssom_selector_serialization_roundtrip_keeps_namespace_subject_matches() {
        let document=crate::xml::parse("<d:root xmlns:d='urn:default' xmlns:f='urn:foreign'><d:e id='d' class='x'/><f:e id='f' class='x'/></d:root>",32).unwrap();
        let d=crate::selector::query_selector(&document,document.root(),"#d").unwrap().unwrap();
        let f=crate::selector::query_selector(&document,document.root(),"#f").unwrap().unwrap();
        let map=NamespaceMap::from_stylesheet("@namespace url('urn:default');@namespace foreign url('urn:foreign');").unwrap();
        for (source,expected) in [(":is(*.x)",(true,false)),(":is(.x)",(true,false)),("*|*:is(.x)",(true,true)),("*|*:is(*.x)",(true,false)),("*.x",(true,false)),("foreign|*.x",(false,true)),(":not(*.x)",(false,false)),("*|*:not(*.x)",(false,true)),("*|*:not(.x)",(false,false))] {
            let original=parse_selector_depth(source,0,0,false,&map).unwrap();
            let text=serialize(source,&map,false).unwrap();let serialized=parse_selector_depth(&text,0,0,false,&map).unwrap();
            assert_eq!((original.matches_node(&document,d),original.matches_node(&document,f)),expected,"{source}");
            assert_eq!((serialized.matches_node(&document,d),serialized.matches_node(&document,f)),expected,"{text}");
        }
    }
    #[test]
    fn specification_cssom_selector_lang_extended_filtering_serialization_and_dom_changes() {
        let mut document=crate::html::parse("<div lang='de-Latn-DE-1996'><span id=target></span></div>",32).unwrap();
        let target=crate::selector::query_selector(&document,document.root(),"#target").unwrap().unwrap();
        let parent=document.parent(target).unwrap().unwrap();
        let map=NamespaceMap::default();
        for (range,expected) in [("de-DE",true),("de-*-DE",true),("*-DE",true),("*-Latn",true),("de",true),("fr",false),("*",true),("",false)] {
            let source=format!(":lang({})",serialize_string(range));
            let selector=parse_selector_depth(&source,0,0,false,&map).unwrap();
            let serialized=serialize(&source,&map,false).unwrap();
            assert_eq!(selector.matches_node(&document,target),expected,"{range}");
            assert_eq!(parse_selector_depth(&serialized,0,0,false,&map).unwrap().matches_node(&document,target),expected,"{serialized}");
        }
        for (language,range,expected) in [("de-x-DE","de-DE",false),("de-x-DE","de-x",true),("de-a-DE","de-DE",false),("de-a-DE","de-a",true),("fr-ninechars","fr",false),("en","*-en",false),("und","*",true),("","*",false),("","",true),("åå","åå",false)] {
            document.set_attribute(parent,"lang",language).unwrap();
            let source=format!(":lang({})",serialize_string(range));
            assert_eq!(parse_selector_depth(&source,0,0,false,&map).unwrap().matches_node(&document,target),expected,"{language}/{range}");
        }
        let mut work=0;
        assert!(!crate::language::matches_range("de-Latn-DE","de-DE",||{work+=1;work<=2}),"shared selector work budget bounds filtering");
    }
}
