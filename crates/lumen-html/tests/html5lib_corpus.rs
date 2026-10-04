//! WPT's pinned html5lib tree-construction corpus. Profile exclusions are
//! selected before parsing, never from the implementation's result.
use lumen_common::json::Value;
use lumen_html::html::ConformanceToken;
use lumen_html::{html, Document, Namespace, NodeId, NodeKind};
use std::{fmt::Write, path::PathBuf};

fn markup_end(bytes: &[u8], mut index: usize) -> usize {
    let mut quote = None;
    while let Some(&byte) = bytes.get(index) {
        match (quote, byte) {
            (Some(delimiter), byte) if delimiter == byte => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return index + 1,
            _ => {}
        }
        index += 1;
    }
    bytes.len()
}

fn comment_end(bytes: &[u8], mut index: usize) -> usize {
    #[derive(Clone, Copy)]
    enum State {
        Start,
        StartDash,
        Comment,
        EndDash,
        End,
        EndBang,
    }

    let mut state = State::Start;
    while let Some(&byte) = bytes.get(index) {
        use State::*;
        let next = match (state, byte) {
            (Start, b'>') | (StartDash, b'>') | (End, b'>') | (EndBang, b'>') => {
                return index + 1;
            }
            (Start, b'-') => StartDash,
            (StartDash, b'-') => End,
            (Start, _) | (StartDash, _) => Comment,
            (Comment, b'-') => EndDash,
            (Comment, _) => Comment,
            (EndDash, b'-') => End,
            (EndDash, _) => Comment,
            (End, b'!') => EndBang,
            (End, b'-') => End,
            (End, _) => Comment,
            (EndBang, b'-') => EndDash,
            (EndBang, _) => Comment,
        };
        state = next;
        index += 1;
    }
    bytes.len()
}

fn is_appropriate_end_tag(bytes: &[u8], index: usize, name: &[u8]) -> bool {
    bytes.get(index) == Some(&b'<')
        && bytes.get(index + 1) == Some(&b'/')
        && bytes
            .get(index + 2..index + 2 + name.len())
            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
        && bytes
            .get(index + 2 + name.len())
            .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
}

fn appropriate_end_tag(bytes: &[u8], from: usize, name: &[u8]) -> Option<usize> {
    let mut index = from;
    while index < bytes.len() {
        if is_appropriate_end_tag(bytes, index, name) {
            return Some(index);
        }
        index += 1;
    }
    None
}

/// Script data has escaped and double-escaped tokenizer states. A `</script>`
/// spelling inside a double-escaped section is text and only returns the
/// tokenizer to the escaped state; it does not close the element.
fn script_end_tag(bytes: &[u8], mut index: usize) -> Option<usize> {
    #[derive(Clone, Copy)]
    enum State {
        Data,
        Escaped,
        DoubleEscaped,
    }

    let mut state = State::Data;
    while index < bytes.len() {
        match state {
            State::Data if bytes.get(index..index + 4) == Some(b"<!--") => {
                state = State::Escaped;
                index += 4;
            }
            State::Data if is_appropriate_end_tag(bytes, index, b"script") => return Some(index),
            State::Escaped if bytes.get(index..index + 3) == Some(b"-->") => {
                state = State::Data;
                index += 3;
            }
            State::Escaped if is_appropriate_end_tag(bytes, index, b"script") => return Some(index),
            State::Escaped
                if bytes.get(index) == Some(&b'<')
                    && bytes.get(index + 1..index + 7).is_some_and(|name| {
                        name.eq_ignore_ascii_case(b"script")
                            && bytes
                                .get(index + 7)
                                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
                    }) =>
            {
                state = State::DoubleEscaped;
                index += 7;
            }
            State::DoubleEscaped if bytes.get(index..index + 2) == Some(b"</") => {
                if bytes
                    .get(index + 2..index + 8)
                    .is_some_and(|name| name.eq_ignore_ascii_case(b"script"))
                    && bytes
                        .get(index + 8)
                        .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
                {
                    state = State::Escaped;
                    index += 8;
                } else {
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    None
}

/// Lex actual start-tag names while skipping comments, quoted tag attributes,
/// and tokenizer raw-text/RCDATA contents. This keeps profile filters from
/// treating strings such as `<!-- <font> -->` or `<script>"<svg>"</script>`
/// as markup.
fn input_markup(input: &str) -> (Vec<String>, bool, bool) {
    let bytes = input.as_bytes();
    let mut names = Vec::new();
    let mut has_doctype = false;
    let mut has_standard_html_doctype = false;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'<' {
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(b"<!--") {
            index = comment_end(bytes, index + 4);
            continue;
        }
        if bytes[index..]
            .get(..9)
            .is_some_and(|tag| tag.eq_ignore_ascii_case(b"<!doctype"))
        {
            // A U+003E in a quoted public/system identifier terminates the
            // doctype token abruptly in the current tokenizer algorithm.
            let end = bytes[index + 9..]
                .iter()
                .position(|byte| *byte == b'>')
                .map_or(bytes.len(), |offset| index + 9 + offset + 1);
            if !has_doctype {
                has_doctype = true;
                let declaration_end = end.saturating_sub(1);
                has_standard_html_doctype = declaration_end >= index + 9
                    && input[index + 9..declaration_end]
                        .trim()
                        .eq_ignore_ascii_case("html");
            }
            index = end;
            continue;
        }
        if bytes
            .get(index + 1)
            .is_some_and(|byte| *byte == b'!' || *byte == b'?')
        {
            index = markup_end(bytes, index + 2);
            continue;
        }
        if bytes.get(index + 1) == Some(&b'/') {
            index = markup_end(bytes, index + 2);
            continue;
        }
        if !bytes.get(index + 1).is_some_and(u8::is_ascii_alphabetic) {
            index += 1;
            continue;
        }

        let start = index + 1;
        let mut end = start;
        while let Some(&byte) = bytes.get(end) {
            if byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>') {
                break;
            }
            end += 1;
        }
        let name = input[start..end].to_ascii_lowercase();
        names.push(name.clone());
        index = markup_end(bytes, end);
        if matches!(
            name.as_str(),
            "script"
                | "style"
                | "title"
                | "textarea"
                | "iframe"
                | "xmp"
                | "listing"
                | "noembed"
                | "noframes"
        ) {
            let close = if name == "script" {
                script_end_tag(bytes, index)
            } else {
                appropriate_end_tag(bytes, index, name.as_bytes())
            };
            if let Some(close) = close {
                index = close;
            } else {
                break;
            }
        } else if name == "plaintext" {
            break;
        }
    }
    (names, has_doctype, has_standard_html_doctype)
}

fn double_unescape(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut position = 0;
    while position < bytes.len() {
        if bytes.get(position..position + 2) == Some(b"\\u")
            && bytes
                .get(position + 2..position + 6)
                .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
        {
            let digits = core::str::from_utf8(&bytes[position + 2..position + 6]).unwrap();
            let code_unit = u32::from_str_radix(digits, 16).unwrap();
            let mut consumed = 6;
            let code_point = if (0xd800..=0xdbff).contains(&code_unit)
                && bytes.get(position + 6..position + 8) == Some(b"\\u")
                && bytes
                    .get(position + 8..position + 12)
                    .is_some_and(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            {
                let low = core::str::from_utf8(&bytes[position + 8..position + 12]).unwrap();
                let low = u32::from_str_radix(low, 16).unwrap();
                if (0xdc00..=0xdfff).contains(&low) {
                    consumed = 12;
                    0x10000 + ((code_unit - 0xd800) << 10) + (low - 0xdc00)
                } else {
                    0xfffd
                }
            } else if (0xd800..=0xdfff).contains(&code_unit) {
                0xfffd
            } else {
                code_unit
            };
            output.push(char::from_u32(code_point).unwrap_or('\u{fffd}'));
            position += consumed;
        } else {
            let character = input[position..].chars().next().unwrap();
            output.push(character);
            position += character.len_utf8();
        }
    }
    output
}

fn expected_string(value: &Value, double_escaped: bool) -> Option<String> {
    let value = value.as_str()?;
    Some(if double_escaped {
        double_unescape(value)
    } else {
        value.to_string()
    })
}

fn expected_optional_string(value: &Value, double_escaped: bool) -> Option<Option<String>> {
    match value {
        Value::Null => Some(None),
        Value::Str(value) => Some(Some(if double_escaped {
            double_unescape(value)
        } else {
            value.clone()
        })),
        _ => None,
    }
}

fn tokenizer_token_matches(
    actual: &ConformanceToken,
    expected: &Value,
    double_escaped: bool,
) -> bool {
    let Value::Arr(fields) = expected else {
        return false;
    };
    let Some(kind) = fields.first().and_then(Value::as_str) else {
        return false;
    };
    match (actual, kind) {
        (ConformanceToken::Character(text), "Character")
        | (ConformanceToken::Comment(text), "Comment") => {
            fields.len() == 2
                && expected_string(&fields[1], double_escaped).as_deref() == Some(text)
        }
        (ConformanceToken::ProcessingInstruction { target, data }, "ProcessingInstruction") => {
            fields.len() == 3
                && expected_string(&fields[1], double_escaped).as_deref() == Some(target)
                && expected_string(&fields[2], double_escaped).as_deref() == Some(data)
        }
        (
            ConformanceToken::Doctype {
                name,
                public_id,
                system_id,
                correct,
            },
            "DOCTYPE",
        ) => {
            fields.len() == 5
                && expected_optional_string(&fields[1], double_escaped).as_ref() == Some(name)
                && expected_optional_string(&fields[2], double_escaped).as_ref() == Some(public_id)
                && expected_optional_string(&fields[3], double_escaped).as_ref() == Some(system_id)
                && matches!(fields[4], Value::Bool(expected) if expected == *correct)
        }
        (
            ConformanceToken::StartTag {
                name,
                attributes,
                self_closing,
            },
            "StartTag",
        ) => {
            if !(fields.len() == 3 || fields.len() == 4)
                || expected_string(&fields[1], double_escaped).as_deref() != Some(name)
            {
                return false;
            }
            let Value::Obj(expected_attributes) = &fields[2] else {
                return false;
            };
            let mut actual_attributes = attributes.clone();
            actual_attributes.sort();
            let mut expected_attributes = expected_attributes
                .iter()
                .filter_map(|(key, value)| {
                    expected_string(value, double_escaped).map(|value| {
                        (
                            if double_escaped {
                                double_unescape(key)
                            } else {
                                key.clone()
                            },
                            value,
                        )
                    })
                })
                .collect::<Vec<_>>();
            expected_attributes.sort();
            actual_attributes == expected_attributes
                && match fields.get(3) {
                    Some(Value::Bool(true)) => *self_closing,
                    Some(Value::Bool(false)) => !*self_closing,
                    None => !*self_closing,
                    _ => false,
                }
        }
        (ConformanceToken::EndTag(name), "EndTag") => {
            fields.len() == 2
                && expected_string(&fields[1], double_escaped).as_deref() == Some(name)
        }
        _ => false,
    }
}

fn expected_element_with_attribute(expected: &str, name: &str) -> bool {
    expected.lines().any(|line| {
        let Some(tree_line) = line.trim_start().strip_prefix('|') else {
            return false;
        };
        let Some(element) = tree_line.trim_start().strip_prefix('<') else {
            return false;
        };
        element
            .strip_prefix(name)
            .is_some_and(|rest| rest.starts_with(' '))
    })
}

fn tree(document: &Document, parent: NodeId, depth: usize, out: &mut String) {
    let mut child = document.first_child(parent).unwrap();
    while let Some(id) = child {
        let prefix = format!("| {}", "  ".repeat(depth));
        match document.kind(id).unwrap() {
            NodeKind::Element {
                name,
                namespace: Namespace::Html,
                attributes,
            } => {
                writeln!(out, "{prefix}<{name}>").unwrap();
                let mut attributes: Vec<_> = attributes.iter().collect();
                attributes.sort_by(|a, b| a.0.cmp(&b.0));
                for (name, value) in attributes {
                    writeln!(out, "{prefix}  {name}=\"{value}\"").unwrap();
                }
                if let Some(content) = document.template_content(id).unwrap() {
                    writeln!(out, "{prefix}  content").unwrap();
                    tree(document, content, depth + 2, out);
                } else {
                    tree(document, id, depth + 1, out);
                }
            }
            NodeKind::Text(value) => {
                writeln!(out, "{prefix}\"{value}\"").unwrap();
            }
            NodeKind::Comment(value) => {
                writeln!(out, "{prefix}<!-- {value} -->").unwrap();
            }
            NodeKind::DocumentType(name) => {
                writeln!(out, "{prefix}<!DOCTYPE {name}>").unwrap();
            }
            NodeKind::ProcessingInstruction { target, data } => {
                writeln!(out, "{prefix}<?{target} {data}?>").unwrap();
            }
            other => panic!("unexpected node in restricted HTML tree: {other:?}"),
        }
        child = document.next_sibling(id).unwrap();
    }
}

fn exclusion(input: &str, expected: &str, context: &str, scripting: bool) -> Option<&'static str> {
    if scripting {
        return Some("scripting-enabled tree construction; scripts are inert");
    }
    if expected_element_with_attribute(expected, "svg")
        || expected_element_with_attribute(expected, "math")
        || context.starts_with("svg ")
        || context.starts_with("math ")
    {
        return Some("foreign-content tree construction is outside the profile");
    }
    let (names, has_doctype, has_standard_html_doctype) = input_markup(input);
    for name in names {
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
            return Some("obsolete element rejected by the standards-only profile");
        }
    }
    // The standards-only profile accepts an actual canonical doctype token;
    // text later inside a malformed declaration cannot satisfy this check.
    if has_doctype && !has_standard_html_doctype {
        return Some("nonstandard doctype/quirks mode is outside the profile");
    }
    None
}

fn current_spec_conflict(identity: &str, input: &str, actual: &str) -> Option<&'static str> {
    const EMPTY_P_THEN_TABLE: &str = "| <html>\n|   <head>\n|   <body>\n|     <p>\n|     <table>";
    const EMPTY_P_FOSTERED_BEFORE_TABLE: &str =
        "| <html>\n|   <head>\n|   <body>\n|     <p>\n|     <p>\n|     <table>";
    const SELECT_INPUT_CHILD: &str =
        "| <html>\n|   <head>\n|   <body>\n|     <select>\n|       <input>";
    const SELECT_INPUT_TEXT_CHILD: &str =
        "| <!DOCTYPE html>\n| <html>\n|   <head>\n|   <body>\n|     <select>\n|       <input>\n|       \"X\"";
    const SELECT_FRAGMENT_INPUT: &str = "| <input>\n| <option>";
    const INCOMPLETE_SCRIPT_END_TAG_CASES: &[&str] = &[
        "tests16.dat:11",
        "tests16.dat:18",
        "tests16.dat:29",
        "tests16.dat:46",
        "tests16.dat:47",
        "tests16.dat:59",
        "tests16.dat:60",
        "tests16.dat:110",
        "tests16.dat:117",
        "tests16.dat:128",
        "tests16.dat:145",
        "tests16.dat:146",
        "tests16.dat:156",
        "tests16.dat:157",
    ];
    if INCOMPLETE_SCRIPT_END_TAG_CASES.contains(&identity)
        && input.to_ascii_lowercase().contains("<script>")
        && actual.to_ascii_lowercase().contains("</script")
    {
        return Some(
            "HTML Standard §13.2.5.17 enters an end-tag attribute state for an appropriate `</script` token, and §13.2.5.32 emits EOF without emitting an unfinished tag; the pinned expected tree closes script at EOF",
        );
    }
    match identity {
        "tests3.dat:24" if actual.trim_end() == EMPTY_P_THEN_TABLE => Some(
            "HTML Standard §13.2.6.4.7 closes an in-scope p before inserting a table; the pinned expected tree nests the table in that p",
        ),
        "tests20.dat:42" if actual.trim_end() == EMPTY_P_FOSTERED_BEFORE_TABLE => Some(
            "HTML Standard §13.2.6.4.7 closes an in-scope p before inserting a table, and §13.2.6.4.9 foster-parents the later unmatched </p>; the pinned expected tree nests both p and table",
        ),
        "tests7.dat:17" | "webkit02.dat:44" if actual.trim_end() == SELECT_INPUT_CHILD => Some(
            "HTML Standard §13.2.4.1 does not define a legacy in-select insertion mode; the select element's input child is handled by §13.2.6.4.7",
        ),
        "tests_innerHTML_1.dat:76" if actual.trim_end() == SELECT_FRAGMENT_INPUT => Some(
            "HTML Standard §13.2.4.1 step 15 resets this select fragment to in-body mode; §13.2.6.4.7 inserts the input token before the option",
        ),
        "tests7.dat:17" if actual.trim_end() == SELECT_INPUT_TEXT_CHILD => Some(
            "HTML Standard §13.2.4.1 does not define a legacy in-select insertion mode; the select element's input and text children are handled by §13.2.6.4.7",
        ),
        _ => None,
    }
}

#[test]
#[ignore = "requires the pinned WPT html5lib corpus"]
fn html5lib_tree_construction_profile() {
    profile_scanner_ignores_markup_inside_comments_attributes_and_raw_text();
    profile_scanner_requires_the_actual_doctype_token_to_be_canonical();
    let directory = PathBuf::from(
        std::env::var_os("LUMEN_HTML5LIB_CORPUS").expect("run cargo xtask test html-corpus"),
    );
    let mut paths: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "dat"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "empty corpus");
    let (mut passed, mut spec_conflicts, mut excluded, mut failed) = (0, 0, 0, 0);
    let mut report = String::new();
    for path in paths {
        let source = std::fs::read_to_string(&path).unwrap();
        for (index, case) in source.split("#data\n").skip(1).enumerate() {
            let (mut input, mut expected, mut context) =
                (String::new(), String::new(), String::new());
            let mut section = "input";
            let mut scripting = false;
            for line in case.split('\n') {
                if line.starts_with('#') {
                    section = line;
                    if line == "#script-on" {
                        scripting = true;
                    }
                } else {
                    let target = match section {
                        "input" => Some(&mut input),
                        "#document" => Some(&mut expected),
                        "#document-fragment" => Some(&mut context),
                        _ => None,
                    };
                    if let Some(target) = target {
                        target.push_str(line);
                        target.push('\n');
                    }
                }
            }
            // Section delimiters introduce a newline that is not input data.
            input.pop();
            let identity = format!(
                "{}:{}",
                path.file_name().unwrap().to_string_lossy(),
                index + 1
            );
            if let Some(reason) = exclusion(&input, &expected, context.trim(), scripting) {
                excluded += 1;
                writeln!(report, "EXCLUDED {identity}: {reason}").unwrap();
                continue;
            }
            let result = if context.is_empty() {
                html::parse(&input, 16384).map(|document| {
                    let root = document.root();
                    (document, root)
                })
            } else {
                let mut document = Document::new(16384);
                let context = document
                    .create(NodeKind::Element {
                        namespace: Namespace::Html,
                        name: context.trim().into(),
                        attributes: Vec::new(),
                    })
                    .unwrap();
                html::parse_fragment_in(&mut document, context, &input).map(|root| (document, root))
            };
            let actual = result.map(|(document, root)| {
                let mut out = String::new();
                tree(&document, root, 0, &mut out);
                out
            });
            match actual {
                Ok(actual) if actual.trim_end() == expected.trim_end() => {
                    passed += 1;
                    writeln!(report, "PASS {identity}").unwrap();
                }
                Ok(actual) if current_spec_conflict(&identity, &input, &actual).is_some() => {
                    spec_conflicts += 1;
                    let basis = current_spec_conflict(&identity, &input, &actual).unwrap();
                    writeln!(
                        report,
                        "SPEC-CONFLICT {identity}\nINPUT {input:?}\nPINNED EXPECTED\n{expected}\nCURRENT HTML STANDARD TREE\n{actual}\nSPEC BASIS: {basis}"
                    )
                    .unwrap();
                }
                other => {
                    failed += 1;
                    writeln!(
                        report,
                        "FAIL {identity}\nINPUT {input:?}\nEXPECTED\n{expected}\nACTUAL\n{}",
                        other.unwrap_or_else(|error| format!("ERROR {error:?}"))
                    )
                    .unwrap();
                }
            }
        }
    }
    eprintln!(
        "html5lib profile: {passed} passed, {spec_conflicts} spec conflicts, {failed} failed, {excluded} excluded"
    );
    let output = PathBuf::from(
        std::env::var_os("LUMEN_HTML_CORPUS_REPORT").expect("corpus report destination missing"),
    );
    std::fs::write(&output, report).unwrap();
    assert_eq!(failed, 0, "see {}", output.display());
    run_html5lib_tokenizer_corpus();
    serializer_roundtrip_profile();
}

fn serializer_roundtrip_profile() {
    let source = "<!doctype html><!--roundtrip--><html lang='en'><head><title>A &amp; B</title><style>p { color: red }</style></head><body><p title='&quot;x&quot;'>One &lt; two &amp; three</p><table><tr><td>A</td><td>B</td></tr></table><template><i>inside</i></template><script>const amp = '&';</script></body></html>";
    let document = html::parse(source, 256).unwrap();
    let serialized = html::outer_html(&document, document.root()).unwrap();
    let reparsed = html::parse(&serialized, 256).unwrap();
    let mut original_tree = String::new();
    let mut reparsed_tree = String::new();
    tree(&document, document.root(), 0, &mut original_tree);
    tree(&reparsed, reparsed.root(), 0, &mut reparsed_tree);
    assert_eq!(
        reparsed_tree, original_tree,
        "document serializer round-trip"
    );
    assert_eq!(
        html::outer_html(&reparsed, reparsed.root()).unwrap(),
        serialized,
        "document serialization should be stable after reparsing"
    );

    let source = "<!--fragment--><p data-value='&quot;&amp;'>A &lt; B</p><table><tr><td>C</td></tr></table><template><b>D</b></template>";
    let mut document = Document::new(256);
    let fragment = html::parse_fragment(&mut document, source).unwrap();
    let serialized = html::inner_html(&document, fragment).unwrap();
    let mut reparsed_document = Document::new(256);
    let reparsed_fragment = html::parse_fragment(&mut reparsed_document, &serialized).unwrap();
    let mut original_tree = String::new();
    let mut reparsed_tree = String::new();
    tree(&document, fragment, 0, &mut original_tree);
    tree(&reparsed_document, reparsed_fragment, 0, &mut reparsed_tree);
    assert_eq!(
        reparsed_tree, original_tree,
        "fragment serializer round-trip"
    );
    assert_eq!(
        html::inner_html(&reparsed_document, reparsed_fragment).unwrap(),
        serialized,
        "fragment serialization should be stable after reparsing"
    );
    eprintln!("restricted HTML serializer round-trips: 2 passed");
}

fn run_html5lib_tokenizer_corpus() {
    let directory = std::env::var_os("LUMEN_HTML_TOKENIZER_CORPUS")
        .map(PathBuf::from)
        .or_else(|| {
            let tree_corpus = PathBuf::from(std::env::var_os("LUMEN_HTML5LIB_CORPUS")?);
            let corpus_root = tree_corpus
                .ancestors()
                .find(|path| path.file_name().is_some_and(|name| name == "corpora"))?;
            Some(corpus_root.join("html5lib-tests/tokenizer"))
        })
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../../target/corpora/html5lib-tests/tokenizer")
        });
    let mut paths: Vec<_> = std::fs::read_dir(&directory)
        .unwrap_or_else(|error| {
            panic!(
                "missing tokenizer corpus at {}: {error}",
                directory.display()
            )
        })
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "test"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "empty tokenizer corpus");

    let (mut passed, mut failed, mut excluded, mut errors_unchecked) = (0, 0, 0, 0);
    let mut report = String::new();
    for path in paths {
        let source = std::fs::read_to_string(&path).unwrap();
        let suite = lumen_common::json::parse(&source)
            .unwrap_or_else(|error| panic!("invalid tokenizer JSON {}: {error}", path.display()));
        let Some(tests) = suite.get("tests") else {
            if suite.get("xmlViolationTests").is_some() {
                excluded += 1;
                writeln!(
                    report,
                    "EXCLUDED {}: xmlViolationTests require XML infoset coercion",
                    path.file_name().unwrap().to_string_lossy()
                )
                .unwrap();
                continue;
            }
            panic!("tokenizer suite has no tests array: {}", path.display());
        };
        let Value::Arr(tests) = tests else {
            panic!("tokenizer tests member is not an array: {}", path.display());
        };
        for (index, test) in tests.iter().enumerate() {
            let identity = format!(
                "{}:{}",
                path.file_name().unwrap().to_string_lossy(),
                index + 1
            );
            let Some(input) = test.get("input").and_then(Value::as_str) else {
                failed += 1;
                writeln!(report, "FAIL {identity}: missing input string").unwrap();
                continue;
            };
            let double_escaped = matches!(test.get("doubleEscaped"), Some(Value::Bool(true)));
            let input = if double_escaped {
                double_unescape(input)
            } else {
                input.to_string()
            };
            let Some(expected) = test.get("output") else {
                failed += 1;
                writeln!(report, "FAIL {identity}: missing output array").unwrap();
                continue;
            };
            let states = match test.get("initialStates") {
                Some(Value::Arr(states)) => states
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
                _ => vec!["Data state".to_string()],
            };
            let last_start_tag = test.get("lastStartTag").and_then(Value::as_str);
            if matches!(test.get("errors"), Some(Value::Arr(errors)) if !errors.is_empty()) {
                errors_unchecked += states.len();
            }
            for state in states {
                let label = format!("{identity} [{state}]");
                match html::tokenize_for_conformance(&input, &state, last_start_tag) {
                    Ok(actual) => {
                        let Value::Arr(expected_tokens) = expected else {
                            failed += 1;
                            writeln!(report, "FAIL {label}: output is not an array").unwrap();
                            continue;
                        };
                        let mismatch = (0..actual.len().min(expected_tokens.len())).find(|index| {
                            !tokenizer_token_matches(
                                &actual[*index],
                                &expected_tokens[*index],
                                double_escaped,
                            )
                        });
                        if mismatch.is_none() && actual.len() == expected_tokens.len() {
                            passed += 1;
                            writeln!(report, "PASS {label}").unwrap();
                        } else {
                            failed += 1;
                            let at = mismatch.unwrap_or(actual.len().min(expected_tokens.len()));
                            let actual_preview = actual
                                .get(at)
                                .map(|token| format!("{token:?}"))
                                .unwrap_or_else(|| "<end of stream>".to_string());
                            let expected_preview = expected_tokens
                                .get(at)
                                .map(|token| format!("{token:?}"))
                                .unwrap_or_else(|| "<end of stream>".to_string());
                            writeln!(
                                report,
                                "FAIL {label}\nINPUT {:?}\nTOKEN {at}\nEXPECTED {expected_preview}\nACTUAL {actual_preview}\nLENGTHS expected={} actual={}",
                                input.chars().take(160).collect::<String>(),
                                expected_tokens.len(),
                                actual.len()
                            )
                            .unwrap();
                        }
                    }
                    Err(error) => {
                        failed += 1;
                        writeln!(report, "FAIL {label}: parser error {error:?}").unwrap();
                    }
                }
            }
        }
    }
    let output = std::env::var_os("LUMEN_HTML_TOKENIZER_REPORT")
        .map(PathBuf::from)
        .or_else(|| {
            let tree_report = PathBuf::from(std::env::var_os("LUMEN_HTML_CORPUS_REPORT")?);
            Some(tree_report.with_file_name("html5lib-tokenizer-results.txt"))
        })
        .unwrap_or_else(|| PathBuf::from("target/html5lib-tokenizer-results.txt"));
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&output, report).unwrap_or_else(|error| {
        panic!(
            "cannot write tokenizer report {}: {error}",
            output.display()
        )
    });
    eprintln!(
        "html5lib tokenizer: {passed} passed, {failed} failed, {excluded} excluded suites; {errors_unchecked} expected parse-error locations not checked"
    );
    assert_eq!(failed, 0, "see {}", output.display());
}

#[test]
fn profile_scanner_ignores_markup_inside_comments_attributes_and_raw_text() {
    let input = "<!-- <font><svg> --> <div title=\"<font>\"></div><script>\"<svg><font>\"</script>";
    let (names, has_doctype, standard_doctype) = input_markup(input);
    assert_eq!(names, ["div", "script"]);
    assert!(!has_doctype);
    assert!(!standard_doctype);
    assert_eq!(exclusion(input, "", "", false), None);

    // In script double-escaped state, the first apparent close tag only
    // returns the tokenizer to escaped state. `<font>` remains script text.
    let script = "<script><!--<script></script><font></script><div></div>";
    assert_eq!(input_markup(script).0, ["script", "div"]);
    assert_eq!(exclusion(script, "", "", false), None);
    assert_eq!(input_markup("<script><!--<script></script><font>").0, ["script"]);

    // `--!>`, `<!-->`, and `<!--->` all terminate HTML comments.
    assert_eq!(input_markup("<!-- <font> --!> <div>").0, ["div"]);
    assert_eq!(
        exclusion("<!-- <font> --!> <div>", "", "", false),
        None
    );
    assert_eq!(input_markup("<!--><font>").0, ["font"]);
    assert_eq!(input_markup("<!---><font>").0, ["font"]);
    assert_eq!(input_markup("<!-----><font>").0, ["font"]);
    assert_eq!(
        exclusion("<!--><font>", "", "", false),
        Some("obsolete element rejected by the standards-only profile")
    );
    assert_eq!(
        exclusion("<!-----><font>", "", "", false),
        Some("obsolete element rejected by the standards-only profile")
    );
}

#[test]
fn profile_scanner_requires_the_actual_doctype_token_to_be_canonical() {
    let nested = "<!DOCTYPE <!DOCTYPE HTML>><p>x";
    assert_eq!(
        exclusion(nested, "", "", false),
        Some("nonstandard doctype/quirks mode is outside the profile")
    );
    assert_eq!(
        exclusion("<!-- <!doctype other> --> <p>x", "", "", false),
        None
    );
    assert_eq!(exclusion("<!doctypehtml><p>x", "", "", false), None);
}
