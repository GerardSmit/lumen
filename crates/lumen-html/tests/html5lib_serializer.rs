//! Pinned html5lib serializer fixtures under the HTML DOM fragment-serialization profile.
use lumen_common::json::Value;
use lumen_html::{html, Document, Name, Namespace, NodeId, NodeKind};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const HTML5LIB_COMMIT: &str = "c777c408b61078ea2eb4acefc2535f54dbc8b28a";
const XHTML_NAMESPACE: &str = "http://www.w3.org/1999/xhtml";

fn serializer_corpus_root() -> PathBuf {
    std::env::var_os("LUMEN_HTML5LIB_SERIALIZER_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(4)
                .expect("Lumen crate must be under external/lumen/crates")
                .join("target/corpora/html5lib-tests")
        })
}

fn verify_pinned_inputs(root: &Path) {
    let head = Command::new("git")
        .args(["-C", root.to_str().unwrap(), "rev-parse", "HEAD"])
        .output()
        .unwrap_or_else(|error| panic!("cannot read html5lib corpus pin: {error}"));
    assert!(
        head.status.success(),
        "html5lib corpus is not a Git checkout"
    );
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        HTML5LIB_COMMIT,
        "serializer/encoding fixtures must use the pinned html5lib revision"
    );

    let paths = ["serializer", "encoding"];
    let mut diff = Command::new("git");
    diff.args([
        "-C",
        root.to_str().unwrap(),
        "diff",
        "--exit-code",
        "HEAD",
        "--",
    ])
    .args(paths);
    let diff = diff
        .output()
        .unwrap_or_else(|error| panic!("cannot verify html5lib fixture contents: {error}"));
    assert!(
        diff.status.success(),
        "pinned serializer/encoding input files changed:\n{}",
        String::from_utf8_lossy(&diff.stdout)
    );

    let mut status = Command::new("git");
    status
        .args([
            "-C",
            root.to_str().unwrap(),
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--",
        ])
        .args(paths);
    let status = status
        .output()
        .unwrap_or_else(|error| panic!("cannot verify html5lib fixture status: {error}"));
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "serializer/encoding fixture paths contain modified or untracked files:\n{}",
        String::from_utf8_lossy(&status.stdout)
    );
}

fn json_string(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len() + 2);
    encoded.push('"');
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            '\0' => encoded.push_str("\\u0000"),
            character if character.is_control() => {
                write!(encoded, "\\u{:04x}", character as u32).unwrap();
            }
            character => encoded.push(character),
        }
    }
    encoded.push('"');
    encoded
}

fn is_void(name: &str) -> bool {
    matches!(
        name,
        "area"
            | "base"
            | "basefont"
            | "bgsound"
            | "br"
            | "col"
            | "embed"
            | "frame"
            | "hr"
            | "img"
            | "input"
            | "keygen"
            | "link"
            | "meta"
            | "param"
            | "source"
            | "track"
            | "wbr"
    )
}

fn optional_tag(name: &str) -> bool {
    matches!(
        name,
        "html"
            | "head"
            | "body"
            | "li"
            | "dt"
            | "dd"
            | "p"
            | "rt"
            | "rp"
            | "optgroup"
            | "option"
            | "colgroup"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "td"
            | "th"
    )
}

fn token_name<'a>(fields: &'a [Value], namespaced: bool) -> Option<&'a str> {
    match (namespaced, fields.len()) {
        (true, 4) => {
            if fields.get(1).and_then(Value::as_str) != Some(XHTML_NAMESPACE) {
                return None;
            }
            fields.get(2).and_then(Value::as_str)
        }
        (false, 3) => fields.get(1).and_then(Value::as_str),
        _ => None,
    }
}

fn attributes_value<'a>(fields: &'a [Value], namespaced: bool) -> Option<&'a Value> {
    match (namespaced, fields.len()) {
        (true, 4) => Some(&fields[3]),
        (false, 3) => Some(&fields[2]),
        _ => None,
    }
}

fn unsupported_attribute_namespace(attributes: &Value) -> bool {
    let namespace_is_unsupported = |attribute: &Value| {
        attribute
            .get("namespace")
            .is_some_and(|namespace| !matches!(namespace, Value::Null))
    };
    match attributes {
        Value::Arr(attributes) => attributes.iter().any(namespace_is_unsupported),
        Value::Obj(attributes) => !attributes.is_empty(),
        _ => false,
    }
}

/// Profile exclusions are determined from corpus metadata and input tokens before parsing or
/// serializing the case. The HTML5lib token serializer has features outside DOM fragment output.
fn profile_exclusion(test: &Value) -> Option<&'static str> {
    if let Some(options) = test.get("options") {
        let only_utf8_encoding = matches!(
            options,
            Value::Obj(options)
                if options.len() == 1
                    && options.iter().any(|(name, value)| {
                        name == "encoding"
                            && value
                                .as_str()
                                .is_some_and(|encoding| encoding.eq_ignore_ascii_case("utf-8"))
                    })
        );
        if !only_utf8_encoding {
            return Some(
                "html5lib token-serializer options change output outside the default DOM fragment profile",
            );
        }
    }
    let Some(Value::Arr(tokens)) = test.get("input") else {
        return None;
    };
    for token in tokens {
        let Value::Arr(fields) = token else {
            continue;
        };
        let Some(kind) = fields.first().and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "StartTag" => {
                let namespaced = fields.len() == 4;
                let Some(name) = token_name(fields, namespaced) else {
                    return Some("non-HTML element token namespaces are outside this profile");
                };
                if optional_tag(name) {
                    return Some(
                        "html5lib optional-tag omission cases are outside DOM fragment serialization",
                    );
                }
                let Some(attributes) = attributes_value(fields, namespaced) else {
                    continue;
                };
                if unsupported_attribute_namespace(attributes) {
                    return Some(
                        "namespaced serializer attributes are outside the current DOM attribute model",
                    );
                }
            }
            "EmptyTag" => {
                let Some(name) = token_name(fields, false) else {
                    return Some("malformed empty-tag token shape");
                };
                if !is_void(name) {
                    return Some(
                        "non-void EmptyTag tokens have no equivalent HTML DOM element serialization",
                    );
                }
                let Some(attributes) = attributes_value(fields, false) else {
                    continue;
                };
                if unsupported_attribute_namespace(attributes) {
                    return Some(
                        "namespaced serializer attributes are outside the current DOM attribute model",
                    );
                }
            }
            "EndTag" => {
                if fields.len() == 3
                    && fields.get(1).and_then(Value::as_str) != Some(XHTML_NAMESPACE)
                {
                    return Some("non-HTML end-tag token namespaces are outside this profile");
                }
                let Some(name) = fields.last().and_then(Value::as_str) else {
                    return Some("malformed end-tag token shape");
                };
                if optional_tag(name) {
                    return Some(
                        "html5lib optional-tag omission cases are outside DOM fragment serialization",
                    );
                }
            }
            "Doctype" => {
                return Some(
                    "document type nodes are not valid children of a DOM fragment; covered by document serializer tests",
                );
            }
            "Characters" | "Comment" | "ProcessingInstruction" => {}
            _ => return Some("unsupported html5lib serializer token kind"),
        }
    }
    None
}

fn attributes(value: &Value) -> Result<Vec<(Name, String)>, String> {
    match value {
        Value::Arr(attributes) => attributes
            .iter()
            .map(|attribute| {
                let name = attribute
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "attribute token has no string name".to_string())?;
                let value = attribute
                    .get("value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "attribute token has no string value".to_string())?;
                Ok((Name::new(name), value.to_string()))
            })
            .collect(),
        Value::Obj(attributes) => attributes
            .iter()
            .map(|(name, value)| {
                let value = value
                    .as_str()
                    .ok_or_else(|| "attribute map contains a non-string value".to_string())?;
                Ok((Name::new(name), value.to_string()))
            })
            .collect(),
        _ => Err("start-tag attributes are neither an array nor an object".to_string()),
    }
}

fn fragment_from_tokens(tokens: &Value) -> Result<(Document, NodeId), String> {
    let Value::Arr(tokens) = tokens else {
        return Err("input is not an array of serializer tokens".to_string());
    };
    let mut document = Document::new(32_768);
    let fragment = document
        .create(NodeKind::DocumentFragment)
        .map_err(|error| format!("create fragment: {error:?}"))?;
    let mut parent = fragment;
    let mut open: Vec<(String, NodeId)> = Vec::new();
    for token in tokens {
        let Value::Arr(fields) = token else {
            return Err("serializer token is not an array".to_string());
        };
        let kind = fields
            .first()
            .and_then(Value::as_str)
            .ok_or_else(|| "serializer token has no kind".to_string())?;
        match kind {
            "StartTag" | "EmptyTag" => {
                let namespaced = kind == "StartTag" && fields.len() == 4;
                let name = token_name(fields, namespaced)
                    .ok_or_else(|| format!("invalid {kind} token name"))?;
                let attributes = attributes(
                    attributes_value(fields, namespaced)
                        .ok_or_else(|| format!("invalid {kind} token attributes"))?,
                )?;
                let element = document
                    .create(NodeKind::Element {
                        namespace: Namespace::Html,
                        name: Name::new(name),
                        attributes,
                    })
                    .map_err(|error| format!("create <{name}>: {error:?}"))?;
                document
                    .append(parent, element)
                    .map_err(|error| format!("append <{name}>: {error:?}"))?;
                if kind == "StartTag" && !is_void(name) {
                    open.push((name.to_string(), parent));
                    parent = document
                        .template_content(element)
                        .map_err(|error| format!("read template content: {error:?}"))?
                        .unwrap_or(element);
                }
            }
            "EndTag" => {
                let name = fields
                    .last()
                    .and_then(Value::as_str)
                    .ok_or_else(|| "invalid EndTag token name".to_string())?;
                let Some((open_name, previous_parent)) = open.pop() else {
                    return Err(format!("unmatched </{name}> token"));
                };
                if open_name != name {
                    return Err(format!("</{name}> closes <{open_name}> out of order"));
                }
                parent = previous_parent;
            }
            "Characters" | "Comment" => {
                let data = fields
                    .get(1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("{kind} token has no string data"))?;
                let node = document
                    .create(if kind == "Characters" {
                        NodeKind::Text(data.to_string())
                    } else {
                        NodeKind::Comment(data.to_string())
                    })
                    .map_err(|error| format!("create {kind}: {error:?}"))?;
                document
                    .append(parent, node)
                    .map_err(|error| format!("append {kind}: {error:?}"))?;
            }
            "ProcessingInstruction" => {
                let target = fields
                    .get(1)
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ProcessingInstruction token has no target".to_string())?;
                let data = fields
                    .get(2)
                    .and_then(Value::as_str)
                    .ok_or_else(|| "ProcessingInstruction token has no data".to_string())?;
                let node = document
                    .create(NodeKind::ProcessingInstruction {
                        target: target.to_string(),
                        data: data.to_string(),
                    })
                    .map_err(|error| format!("create PI: {error:?}"))?;
                document
                    .append(parent, node)
                    .map_err(|error| format!("append PI: {error:?}"))?;
            }
            _ => return Err(format!("unsupported token kind {kind}")),
        }
    }
    Ok((document, fragment))
}

fn canonical_fragment(source: &str) -> Result<String, String> {
    let mut document = Document::new(32_768);
    let fragment = html::parse_fragment(&mut document, source)
        .map_err(|error| format!("parse fragment: {error:?}"))?;
    html::inner_html(&document, fragment).map_err(|error| format!("serialize fragment: {error:?}"))
}

fn serializer_semantic_profile() {
    let mut document = Document::new(256);
    let fragment = document.create(NodeKind::DocumentFragment).unwrap();
    let title = "A\u{00a0}\0<&>\"";
    let element = document
        .create(NodeKind::Element {
            namespace: Namespace::Html,
            name: Name::new("div"),
            attributes: vec![(Name::new("title"), title.to_string())],
        })
        .unwrap();
    document.append(fragment, element).unwrap();
    let text = "A\u{00a0}\0<&>";
    let text_node = document.create(NodeKind::Text(text.to_string())).unwrap();
    document.append(element, text_node).unwrap();
    assert_eq!(
        html::outer_html(&document, element).unwrap(),
        "<div title=\"A&nbsp;\0&lt;&amp;&gt;&quot;\">A&nbsp;\0&lt;&amp;&gt;</div>"
    );
    assert_eq!(
        html::inner_html(&document, element).unwrap(),
        "A&nbsp;\0&lt;&amp;&gt;"
    );

    let mut document = Document::new(16);
    let image = document
        .create(NodeKind::Element {
            namespace: Namespace::Html,
            name: Name::new("img"),
            attributes: Vec::new(),
        })
        .unwrap();
    document.append(document.root(), image).unwrap();
    let stray_child = document
        .create(NodeKind::Text("ignored".to_string()))
        .unwrap();
    document.append(image, stray_child).unwrap();
    assert_eq!(html::inner_html(&document, image).unwrap(), "");

    for name in [
        "style",
        "script",
        "xmp",
        "iframe",
        "noembed",
        "noframes",
        "plaintext",
    ] {
        let mut document = Document::new(16);
        let element = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new(name),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(document.root(), element).unwrap();
        let text = document
            .create(NodeKind::Text("<raw>&\u{00a0}\0".to_string()))
            .unwrap();
        document.append(element, text).unwrap();
        assert_eq!(
            html::outer_html(&document, element).unwrap(),
            format!("<{name}><raw>&\u{00a0}\0</{name}>")
        );
    }
    for name in ["title", "textarea", "noscript"] {
        let mut document = Document::new(16);
        let element = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: Name::new(name),
                attributes: Vec::new(),
            })
            .unwrap();
        document.append(document.root(), element).unwrap();
        let text = document
            .create(NodeKind::Text("<rcdata>&\u{00a0}\0".to_string()))
            .unwrap();
        document.append(element, text).unwrap();
        assert_eq!(
            html::outer_html(&document, element).unwrap(),
            format!("<{name}>&lt;rcdata&gt;&amp;&nbsp;\0</{name}>")
        );
    }

    let mut document = Document::new(32);
    let fragment = html::parse_fragment(
        &mut document,
        "<template><b title='A&nbsp;B'>inside</b></template>",
    )
    .unwrap();
    let template = document.first_child(fragment).unwrap().unwrap();
    assert_eq!(
        html::inner_html(&document, template).unwrap(),
        "<b title=\"A&nbsp;B\">inside</b>"
    );
    assert_eq!(
        html::outer_html(&document, template).unwrap(),
        "<template><b title=\"A&nbsp;B\">inside</b></template>"
    );
}

fn record_case(results: &mut Vec<String>, identity: &str, status: &str, detail: &str) {
    results.push(format!(
        "{{\"id\":{},\"status\":{},\"detail\":{}}}",
        json_string(identity),
        json_string(status),
        json_string(detail)
    ));
}

fn encoding_profile(corpus: &Path, cases: &mut Vec<String>) -> usize {
    let encoding = corpus.join("encoding");
    let mut files = Vec::new();
    collect_files(&encoding, &mut files);
    files.sort();
    let mut total = 0;
    for path in files {
        let relative = path.strip_prefix(corpus).unwrap().display().to_string();
        if path.extension().is_some_and(|extension| extension == "dat") {
            let source = std::fs::read(&path).unwrap();
            let source = String::from_utf8_lossy(&source);
            let count = source.lines().filter(|line| *line == "#data").count();
            for index in 0..count {
                let identity = format!("{relative}:{}", index + 1);
                let reason = if relative.contains("/scripted/") {
                    "script-triggered encoding changes require script execution and a byte-stream parser"
                } else {
                    "legacy charset selection/byte decoding requires a byte-oriented API; html::parse accepts UTF-8 str"
                };
                record_case(cases, &identity, "EXCLUDED", reason);
            }
            total += count;
        } else {
            record_case(
                cases,
                &relative,
                "EXCLUDED",
                "encoding-detection heuristics require a byte-oriented API; html::parse accepts UTF-8 str",
            );
        }
    }
    total
}

fn collect_files(directory: &Path, output: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
    {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_files(&path, output);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "dat" || extension == "txt")
        {
            output.push(path);
        }
    }
}

#[test]
#[ignore = "requires the pinned html5lib serializer and encoding corpus"]
fn html5lib_serializer_and_encoding_profile() {
    serializer_semantic_profile();
    let corpus = serializer_corpus_root();
    verify_pinned_inputs(&corpus);
    let serializer = corpus.join("serializer");
    let mut paths: Vec<_> = std::fs::read_dir(&serializer)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", serializer.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "test")
        })
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "empty pinned html5lib serializer corpus");

    let mut cases = Vec::new();
    let mut exclusions = BTreeMap::<String, usize>::new();
    let (mut total, mut passed, mut excluded, mut failed) = (0, 0, 0, 0);
    for path in paths {
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let suite = lumen_common::json::parse(&source)
            .unwrap_or_else(|error| panic!("invalid JSON in {}: {error}", path.display()));
        let Some(Value::Arr(tests)) = suite.get("tests") else {
            panic!("serializer suite has no tests array: {}", path.display());
        };
        for (index, test) in tests.iter().enumerate() {
            total += 1;
            let identity = format!(
                "{}:{}",
                path.file_name().unwrap().to_string_lossy(),
                index + 1
            );
            if let Some(reason) = profile_exclusion(test) {
                excluded += 1;
                *exclusions.entry(reason.to_string()).or_default() += 1;
                record_case(&mut cases, &identity, "EXCLUDED", reason);
                continue;
            }
            let Some(Value::Arr(expected_values)) = test.get("expected") else {
                failed += 1;
                record_case(
                    &mut cases,
                    &identity,
                    "FAIL",
                    "expected output is not a string array",
                );
                continue;
            };
            let Some(expected) = expected_values.first().and_then(Value::as_str) else {
                failed += 1;
                record_case(
                    &mut cases,
                    &identity,
                    "FAIL",
                    "expected output has no first string",
                );
                continue;
            };
            let input = test.get("input").unwrap();
            let actual = fragment_from_tokens(input).and_then(|(document, fragment)| {
                html::inner_html(&document, fragment)
                    .map_err(|error| format!("serialize fragment: {error:?}"))
            });
            let result = actual.and_then(|actual| {
                let actual_canonical = canonical_fragment(&actual)?;
                let expected_canonical = canonical_fragment(expected)?;
                if actual_canonical == expected_canonical {
                    Ok(if actual == expected {
                        "exact serializer output match".to_string()
                    } else {
                        "DOM-equivalent after fixed-quote fragment serialization".to_string()
                    })
                } else {
                    Err(format!(
                        "DOM output differs: actual={actual:?}, expected={expected:?}, actual tree serialization={actual_canonical:?}, expected tree serialization={expected_canonical:?}"
                    ))
                }
            });
            match result {
                Ok(detail) => {
                    passed += 1;
                    record_case(&mut cases, &identity, "PASS", &detail);
                }
                Err(detail) => {
                    failed += 1;
                    record_case(&mut cases, &identity, "FAIL", &detail);
                }
            }
        }
    }
    assert_eq!(total, 230, "pinned serializer fixture count changed");

    let encoding_cases = encoding_profile(&corpus, &mut cases);
    assert_eq!(encoding_cases, 83, "pinned encoding fixture count changed");
    let reasons = exclusions
        .iter()
        .map(|(reason, count)| format!("{}:{}", json_string(reason), count))
        .collect::<Vec<_>>()
        .join(",");
    let cases = cases.join(",\n");
    let report = format!(
        "{{\n  \"corpus_commit\": {},\n  \"profile\": {},\n  \"serializer\": {{\"total\": {total}, \"passed\": {passed}, \"excluded\": {excluded}, \"failed\": {failed}, \"exclusions_by_reason\": {{{reasons}}}}},\n  \"encoding\": {{\"cases_seen\": {encoding_cases}, \"cases_excluded\": {encoding_cases}, \"reason\": {}}},\n  \"cases\": [\n{cases}\n  ]\n}}\n",
        json_string(HTML5LIB_COMMIT),
        json_string(
            "HTML DOM fragment serialization; default options; fixed quoted attributes; UTF-8 string API"
        ),
        json_string(
            "byte-stream charset decoding and scripted encoding changes are outside html::parse(&str)"
        )
    );
    let report_path = std::env::var_os("LUMEN_HTML_SERIALIZER_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            corpus
                .parent()
                .expect("corpora directory has target parent")
                .join("html5lib-serializer-results.json")
        });
    std::fs::write(&report_path, report).unwrap();
    eprintln!(
        "html5lib serializer profile: {passed}/{total} passed, {excluded} explicitly excluded, {failed} failed; encoding: {encoding_cases}/{encoding_cases} explicitly excluded"
    );
    assert_eq!(failed, 0, "see {}", report_path.display());
}
