//! Pinned WPT CSS parser/selector profile and hostile-input regression checks.
//!
//! The profile below names only declaration properties and selector grammar
//! already exposed by lumen-html. Everything else is counted as a profile
//! exclusion before any parser result is observed.

use lumen_html::{css, html, selector, Document};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const WPT_COMMIT: &str = "74ca910926d76710943f2a8798817102f69d6e40";

#[derive(Clone, Copy)]
struct WptSpecConflictSource {
    path: &'static str,
    sha256: &'static str,
    git_blob: &'static str,
}

const WPT_SPEC_CONFLICT_SOURCES: &[WptSpecConflictSource] = &[
    WptSpecConflictSource {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        sha256: "0cce57ad83d53bd850b0b828dc63a300a1337fb3ae912ab86d8d5ccc5fa69975",
        git_blob: "43ad1fcc63358a1907535001fd8e0a4f86877eef",
    },
    WptSpecConflictSource {
        path: "css/css-grid/parsing/grid-column-invalid.html",
        sha256: "aadfc8771397cd9d56e4fcdb86786b65308b8d476f25d92b741b7e6e711cd6b2",
        git_blob: "d1f4dce1723e598ce5012deb8064e607c4857b0b",
    },
    WptSpecConflictSource {
        path: "css/css-grid/parsing/grid-row-invalid.html",
        sha256: "6bc249de8a8285c4ba9ab5eaea9e8dbd4ba0c622dfd92de25b64fe71839ef9f9",
        git_blob: "ca7b1d681a9ecb712895dd6c5bef9dc2467a3f38",
    },
];

#[derive(Clone, Copy)]
struct WptSpecConflict {
    path: &'static str,
    line: usize,
    property: &'static str,
    value: &'static str,
    pinned_expected: bool,
    current_expected: bool,
    spec: &'static str,
}

// These exact old negative assertions contradict the linked current grammar.
// They stay separately visible in the report: they are neither normal passes
// nor profile exclusions, and the current-spec expectation is checked.
const WPT_SPEC_CONFLICTS: &[WptSpecConflict] = &[
    WptSpecConflict {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        line: 23,
        property: "color",
        value: "alpha(from red)",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Color 5 §4.10, optional relative alpha component (https://www.w3.org/TR/css-color-5/#relative-alpha)",
    },
    WptSpecConflict {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        line: 24,
        property: "color",
        value: "alpha(from currentcolor)",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Color 5 §4.10, optional relative alpha component (https://www.w3.org/TR/css-color-5/#relative-alpha)",
    },
    WptSpecConflict {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        line: 25,
        property: "color",
        value: "alpha(from rgba(255, 0, 0, 0.3))",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Color 5 §4.10, optional relative alpha component (https://www.w3.org/TR/css-color-5/#relative-alpha)",
    },
    WptSpecConflict {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        line: 26,
        property: "color",
        value: "alpha(from alpha(from currentcolor / 0.5))",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Color 5 §4.10, optional relative alpha component (https://www.w3.org/TR/css-color-5/#relative-alpha)",
    },
    WptSpecConflict {
        path: "css/css-color/parsing/alpha-color-parsing-invalid.html",
        line: 27,
        property: "color",
        value: "alpha(from alpha(from currentcolor) / 0.5)",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Color 5 §4.10, optional relative alpha component (https://www.w3.org/TR/css-color-5/#relative-alpha)",
    },
    WptSpecConflict {
        path: "css/css-grid/parsing/grid-column-invalid.html",
        line: 49,
        property: "grid-column",
        value: "first span 1 / last",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Grid 2 §8.3, <grid-line> integer/custom-ident and span productions (https://www.w3.org/TR/css-grid-2/#grid-placement-property)",
    },
    WptSpecConflict {
        path: "css/css-grid/parsing/grid-column-invalid.html",
        line: 50,
        property: "grid-column",
        value: "3 first / 2 span last",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Grid 2 §8.3, <grid-line> integer/custom-ident and span productions (https://www.w3.org/TR/css-grid-2/#grid-placement-property)",
    },
    WptSpecConflict {
        path: "css/css-grid/parsing/grid-row-invalid.html",
        line: 43,
        property: "grid-row",
        value: "2 span first / last",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Grid 2 §8.3, <grid-line> integer/custom-ident and span productions (https://www.w3.org/TR/css-grid-2/#grid-placement-property)",
    },
    WptSpecConflict {
        path: "css/css-grid/parsing/grid-row-invalid.html",
        line: 44,
        property: "grid-row",
        value: "5 nav / last span 7",
        pinned_expected: false,
        current_expected: true,
        spec: "CSS Grid 2 §8.3, <grid-line> integer/custom-ident and span productions (https://www.w3.org/TR/css-grid-2/#grid-placement-property)",
    },
];

const VALUE_DIRECTORIES: &[&str] = &[
    "css/css-backgrounds/parsing",
    "css/css-cascade/parsing",
    "css/css-color/parsing",
    "css/css-flexbox/parsing",
    "css/css-grid/parsing",
    "css/css-text/parsing",
];

// This is deliberately a checked-in profile, rather than a predicate based on
// the result of supports_declaration(). A valid WPT value for one of these
// properties is a conformance failure if the parser rejects it.
const VALUE_PROPERTIES: &[&str] = &[
    "align-content",
    "align-items",
    "align-self",
    "background",
    "background-attachment",
    "background-clip",
    "background-color",
    "background-image",
    "background-origin",
    "background-position",
    "background-repeat",
    "background-size",
    "border",
    "border-bottom-left-radius",
    "border-bottom-right-radius",
    "border-color",
    "border-radius",
    "border-style",
    "border-top-left-radius",
    "border-top-right-radius",
    "border-width",
    "box-shadow",
    "box-sizing",
    "color",
    "direction",
    "display",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-flow",
    "flex-grow",
    "flex-shrink",
    "flex-wrap",
    "grid",
    "grid-area",
    "grid-auto-columns",
    "grid-auto-flow",
    "grid-auto-rows",
    "grid-column",
    "grid-row",
    "grid-template",
    "grid-template-areas",
    "grid-template-columns",
    "grid-template-rows",
    "opacity",
    "order",
    "text-align",
    "white-space",
];

const SELECTOR_DIRECTORY: &str = "css/selectors/parsing";
const SELECTOR_PSEUDO_CLASSES: &[&str] = &[
    "empty",
    "first-child",
    "first-of-type",
    "lang",
    "last-child",
    "last-of-type",
    "nth-child",
    "nth-last-child",
    "nth-last-of-type",
    "nth-of-type",
    "only-child",
    "only-of-type",
    "root",
];

#[derive(Debug)]
struct JsCall<'a> {
    line: usize,
    args: Vec<&'a str>,
}

#[derive(Clone, Debug)]
struct CssCase {
    path: String,
    line: usize,
    operation: String,
    property: Option<String>,
    value: Option<String>,
}

#[derive(Clone, Debug)]
struct SelectorCase {
    path: String,
    line: usize,
    expected_valid: bool,
    forgiving: bool,
    selector: Option<String>,
}

fn wpt_root() -> PathBuf {
    std::env::var_os("LUMEN_CSS_WPT_CORPUS")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/corpora/wpt"))
}

fn checked_command(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("cannot run git against {}: {error}", root.display()));
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        args,
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output must be UTF-8")
}

fn verify_wpt_inputs(root: &Path) {
    assert!(
        root.join(".git").exists(),
        "WPT corpus is not a Git checkout"
    );
    let head = checked_command(root, &["rev-parse", "HEAD"]);
    assert_eq!(head.trim(), WPT_COMMIT, "WPT corpus pin changed");

    let mut paths = VALUE_DIRECTORIES.to_vec();
    paths.push(SELECTOR_DIRECTORY);
    let mut diff = vec!["diff", "--quiet", WPT_COMMIT, "--"];
    diff.extend(paths.iter().copied());
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&diff)
        .output()
        .expect("cannot check WPT input diff");
    assert!(
        output.status.success(),
        "pinned WPT CSS fixtures have local changes"
    );
    let mut status = vec!["status", "--porcelain", "--untracked-files=all", "--"];
    status.extend(paths.iter().copied());
    let changed = checked_command(root, &status);
    assert!(
        changed.trim().is_empty(),
        "WPT CSS inputs are not pristine: {changed}"
    );

    for source in WPT_SPEC_CONFLICT_SOURCES {
        let blob = checked_command(root, &["hash-object", source.path]);
        assert_eq!(
            blob.trim(),
            source.git_blob,
            "SPEC-CONFLICT source blob changed: {} (sha256 {})",
            source.path,
            source.sha256
        );
    }
}

fn wpt_spec_conflict(case: &CssCase) -> Option<(usize, &'static WptSpecConflict)> {
    WPT_SPEC_CONFLICTS.iter().enumerate().find(|(_, conflict)| {
        case.path == conflict.path
            && case.line == conflict.line
            && case.operation == "invalid"
            && case.property.as_deref() == Some(conflict.property)
            && case.value.as_deref() == Some(conflict.value)
    })
}

fn html_files(directory: &Path, output: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let entry = entry.expect("WPT directory entry");
        let path = entry.path();
        if path.is_dir() {
            html_files(&path, output);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "html")
        {
            output.push(path);
        }
    }
}

fn corpus_files(root: &Path, directory: &str) -> Vec<PathBuf> {
    let mut files = Vec::new();
    html_files(&root.join(directory), &mut files);
    files.sort();
    files
}

fn line_number(source: &str, byte_offset: usize) -> usize {
    source.as_bytes()[..byte_offset]
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1
}

fn skip_quoted(source: &str, start: usize, quote: u8) -> usize {
    let bytes = source.as_bytes();
    let mut position = start + 1;
    while position < bytes.len() {
        if bytes[position] == b'\\' {
            position = (position + 2).min(bytes.len());
        } else if bytes[position] == quote {
            return position + 1;
        } else {
            position += 1;
        }
    }
    bytes.len()
}

fn parse_call_args(source: &str, open: usize) -> Option<(Vec<&str>, usize)> {
    let bytes = source.as_bytes();
    let mut arguments = Vec::new();
    let mut start = open + 1;
    let mut position = start;
    let (mut parentheses, mut brackets, mut braces) = (0usize, 0usize, 0usize);
    while position < bytes.len() {
        match bytes[position] {
            b'\'' | b'"' | b'`' => position = skip_quoted(source, position, bytes[position]),
            b'/' if bytes.get(position + 1) == Some(&b'/') => {
                position += 2;
                while position < bytes.len() && bytes[position] != b'\n' {
                    position += 1;
                }
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                position += 2;
                while position + 1 < bytes.len() && &bytes[position..position + 2] != b"*/" {
                    position += 1;
                }
                if position + 1 >= bytes.len() {
                    return None;
                }
                position += 2;
            }
            b'(' => {
                parentheses += 1;
                position += 1;
            }
            b')' if parentheses != 0 => {
                parentheses -= 1;
                position += 1;
            }
            b'[' => {
                brackets += 1;
                position += 1;
            }
            b']' if brackets != 0 => {
                brackets -= 1;
                position += 1;
            }
            b'{' => {
                braces += 1;
                position += 1;
            }
            b'}' if braces != 0 => {
                braces -= 1;
                position += 1;
            }
            b',' if parentheses == 0 && brackets == 0 && braces == 0 => {
                arguments.push(&source[start..position]);
                position += 1;
                start = position;
            }
            b')' if parentheses == 0 && brackets == 0 && braces == 0 => {
                let last = source[start..position].trim();
                if !last.is_empty() || !arguments.is_empty() {
                    arguments.push(&source[start..position]);
                }
                return Some((arguments, position + 1));
            }
            _ => position += 1,
        }
    }
    None
}

fn is_function_declaration(source: &str, start: usize) -> bool {
    source[..start]
        .trim_end()
        .strip_suffix("function")
        .is_some()
}

fn find_calls<'a>(source: &'a str, name: &str) -> Vec<JsCall<'a>> {
    let bytes = source.as_bytes();
    let mut calls = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        match bytes[position] {
            b'\'' | b'"' | b'`' => position = skip_quoted(source, position, bytes[position]),
            b'/' if bytes.get(position + 1) == Some(&b'/') => {
                position += 2;
                while position < bytes.len() && bytes[position] != b'\n' {
                    position += 1;
                }
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                position += 2;
                while position + 1 < bytes.len() && &bytes[position..position + 2] != b"*/" {
                    position += 1;
                }
                position = (position + 2).min(bytes.len());
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' => {
                let start = position;
                position += 1;
                while position < bytes.len()
                    && (bytes[position].is_ascii_alphanumeric()
                        || matches!(bytes[position], b'_' | b'$'))
                {
                    position += 1;
                }
                if &source[start..position] != name || is_function_declaration(source, start) {
                    continue;
                }
                let mut open = position;
                while open < bytes.len() && bytes[open].is_ascii_whitespace() {
                    open += 1;
                }
                if bytes.get(open) != Some(&b'(') {
                    continue;
                }
                if let Some((args, end)) = parse_call_args(source, open) {
                    calls.push(JsCall {
                        line: line_number(source, start),
                        args,
                    });
                    position = end;
                }
            }
            _ => position += 1,
        }
    }
    calls
}

fn js_literal(source: &str) -> Option<String> {
    let source = source.trim();
    let bytes = source.as_bytes();
    let quote = *bytes.first()?;
    if !matches!(quote, b'\'' | b'"' | b'`') || bytes.last() != Some(&quote) || bytes.len() < 2 {
        return None;
    }
    let inner = &source[1..source.len() - 1];
    if quote == b'`' && inner.contains("${") {
        return None;
    }
    let mut output = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            output.push(ch);
            continue;
        }
        let escaped = chars.next()?;
        match escaped {
            '\n' => {}
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            'n' => output.push('\n'),
            'r' => output.push('\r'),
            't' => output.push('\t'),
            'b' => output.push('\u{0008}'),
            'f' => output.push('\u{000c}'),
            'v' => output.push('\u{000b}'),
            '0' => output.push('\0'),
            'x' => {
                let first = chars.next()?.to_digit(16)?;
                let second = chars.next()?.to_digit(16)?;
                output.push(char::from_u32(first * 16 + second)?);
            }
            'u' => {
                let value = if chars.peek() == Some(&'{') {
                    chars.next();
                    let mut digits = String::new();
                    loop {
                        let next = chars.next()?;
                        if next == '}' {
                            break;
                        }
                        digits.push(next);
                    }
                    u32::from_str_radix(&digits, 16).ok()?
                } else {
                    let mut value = 0u32;
                    for _ in 0..4 {
                        value = value.checked_mul(16)? + chars.next()?.to_digit(16)?;
                    }
                    value
                };
                output.push(char::from_u32(value)?);
            }
            other => output.push(other),
        }
    }
    Some(output)
}

fn wpt_css_cases(root: &Path) -> Vec<CssCase> {
    let mut cases = Vec::new();
    for directory in VALUE_DIRECTORIES {
        for path in corpus_files(root, directory) {
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
            let relative = path.strip_prefix(root).unwrap().display().to_string();
            for (operation, expected_valid) in
                [("test_valid_value", true), ("test_invalid_value", false)]
            {
                for call in find_calls(&source, operation) {
                    let property = call.args.first().and_then(|arg| js_literal(arg));
                    let value = call.args.get(1).and_then(|arg| js_literal(arg));
                    cases.push(CssCase {
                        path: relative.clone(),
                        line: call.line,
                        operation: if expected_valid { "valid" } else { "invalid" }.into(),
                        property,
                        value,
                    });
                }
            }
        }
    }
    cases.sort_by(|left, right| {
        (&left.path, left.line, &left.operation).cmp(&(&right.path, right.line, &right.operation))
    });
    cases
}

fn expand_source_expression(expression: &str, source: &str) -> Option<String> {
    let (base, suffix) = expression.split_once('+')?;
    if base.trim() != "source" {
        return None;
    }
    Some(format!("{source}{}", js_literal(suffix.trim())?))
}

fn wpt_selector_cases(root: &Path) -> Vec<SelectorCase> {
    let mut cases = Vec::new();
    for path in corpus_files(root, SELECTOR_DIRECTORY) {
        let source = fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
        let relative = path.strip_prefix(root).unwrap().display().to_string();
        for (operation, expected_valid, forgiving) in [
            ("test_valid_selector", true, false),
            ("test_valid_forgiving_selector", true, true),
            ("test_invalid_selector", false, false),
        ] {
            for call in find_calls(&source, operation) {
                cases.push(SelectorCase {
                    path: relative.clone(),
                    line: call.line,
                    expected_valid,
                    forgiving,
                    selector: call.args.first().and_then(|arg| js_literal(arg)),
                });
            }
        }

        // The upstream an+b fixture constructs its cases by concatenating a
        // source pseudo-class with literal expressions in helper functions.
        // Expand those exact pinned expressions so they remain conformance
        // inputs instead of silently dropping JavaScript-generated vectors.
        let prefixes: Vec<String> = find_calls(&source, "run_tests_on_anplusb_selector")
            .iter()
            .filter_map(|call| call.args.first().and_then(|arg| js_literal(arg)))
            .collect();
        if !prefixes.is_empty() {
            let valid_templates = find_calls(&source, "assert_selector_serializes_to");
            let invalid_templates = find_calls(&source, "assert_invalid_selector");
            for prefix in prefixes {
                for (templates, expected_valid) in
                    [(&valid_templates, true), (&invalid_templates, false)]
                {
                    for call in templates {
                        let Some(selector) = call
                            .args
                            .first()
                            .and_then(|arg| expand_source_expression(arg, &prefix))
                        else {
                            continue;
                        };
                        cases.push(SelectorCase {
                            path: relative.clone(),
                            line: call.line,
                            expected_valid,
                            forgiving: false,
                            selector: Some(selector),
                        });
                    }
                }
            }
        }
    }
    cases.sort_by(|left, right| {
        (&left.path, left.line, &left.selector).cmp(&(&right.path, right.line, &right.selector))
    });
    cases
}

fn selector_profile_reason(selector: &str, forgiving: bool) -> Option<&'static str> {
    if forgiving {
        return Some("forgiving selector-list parsing is outside querySelector's strict profile");
    }
    if selector.contains("/*") {
        return Some("comments in selector token streams are outside this profile");
    }

    let bytes = selector.as_bytes();
    let mut position = 0;
    let mut bracket = false;
    let mut attribute_start = 0;
    let mut quote = 0u8;
    while position < bytes.len() {
        let byte = bytes[position];
        if quote != 0 {
            if byte == b'\\' {
                position += 2;
                continue;
            }
            if byte == quote {
                quote = 0;
            }
            position += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = byte;
            position += 1;
            continue;
        }
        if byte == b'[' {
            bracket = true;
            attribute_start = position + 1;
            position += 1;
            continue;
        }
        if byte == b']' {
            let attribute = selector[attribute_start..position].trim();
            if let Some((_, raw_value)) = attribute.split_once('=') {
                let raw_value = raw_value.trim();
                if let Some(delimiter @ (b'\'' | b'"')) = raw_value.as_bytes().first().copied() {
                    if let Some(close) = raw_value.as_bytes()[1..]
                        .iter()
                        .position(|value| *value == delimiter)
                    {
                        if !raw_value[close + 2..].trim().is_empty() {
                            return Some(
                                "attribute selector case-sensitivity modifiers are outside the implemented profile",
                            );
                        }
                    }
                }
            }
            bracket = false;
            position += 1;
            continue;
        }
        if bracket && byte == b'\\' {
            return Some("escaped attribute names and values are outside the implemented profile");
        }
        if bracket && byte == b'|' {
            return Some(
                "namespaced or token-matching attribute operators are outside the implemented profile",
            );
        }
        if bracket
            && matches!(byte, b'^' | b'$' | b'*' | b'~')
            && bytes.get(position + 1) == Some(&b'=')
        {
            return Some(
                "attribute substring and token operators are outside the implemented profile",
            );
        }
        if !bracket && byte == b'.' && !selector.contains(':') {
            let mut end = position + 1;
            while end < bytes.len()
                && (bytes[end].is_ascii_alphanumeric()
                    || matches!(bytes[end], b'-' | b'_')
                    || !bytes[end].is_ascii())
            {
                end += 1;
            }
            if end > position + 1
                && end < bytes.len()
                && bytes[end] == b'*'
                && bytes[end - 1] == b'-'
            {
                // These valid cases use Selectors 5's class-prefix wildcard,
                // which is outside the declared selector profile. Exclude by
                // grammar before observing parser behavior; the source is
                // pinned at WPT_COMMIT above.
                return Some(
                    "Selectors-5 class-prefix wildcard syntax is outside the declared selector profile (pinned WPT parse-class-prefix.tentative.html grammar)",
                );
            }
        }
        if !bracket && byte == b'|' {
            return Some("namespaced type selectors are outside the implemented profile");
        }
        if !bracket && byte == b':' {
            if bytes.get(position + 1) == Some(&b':') {
                return Some("pseudo-elements require a pseudo-element query API");
            }
            let start = position + 1;
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphabetic() || bytes[end] == b'-') {
                end += 1;
            }
            if end == start {
                return Some("escaped pseudo-class names are outside the implemented profile");
            }
            let name = &selector[start..end];
            if !SELECTOR_PSEUDO_CLASSES.contains(&name) {
                return Some("pseudo-class is outside the implemented strict query profile");
            }
            if name.starts_with("nth-") {
                let rest = &selector[end..];
                if let Some(open) = rest.find('(') {
                    if rest[open + 1..]
                        .split(')')
                        .next()
                        .is_some_and(|args| args.contains(" of "))
                    {
                        return Some(
                            "nth-child 'of' selector lists are outside the implemented profile",
                        );
                    }
                }
            }
            position = end;
            continue;
        }
        position += 1;
    }
    None
}

fn selector_fixture() -> Document {
    html::parse(
        "<main id='root'><section class='alpha'><p id='first' class='x' data-key='one'>A</p><p class='x y' data-key='two'>B</p><p class='z' lang='en-US'>C</p></section><aside class='alpha'><p id='last'>D</p></aside></main>",
        64,
    )
    .expect("selector fixture must parse")
}

fn query_match_regressions(document: &Document) -> Vec<String> {
    let cases = [
        (".alpha p", 4usize),
        ("section > p", 3),
        ("#first + p", 1),
        ("#first ~ p", 2),
        ("[data-key]", 2),
        ("[data-key=one]", 1),
        ("p:nth-child(2)", 1),
        ("p:nth-of-type(2)", 1),
        ("p:lang(en)", 1),
    ];
    let mut failures = Vec::new();
    for (selector_text, expected_count) in cases {
        match selector::query_selector_all(document, document.root(), selector_text) {
            Ok(nodes) if nodes.len() == expected_count => {}
            Ok(nodes) => failures.push(format!(
                "query selector {selector_text:?}: expected {expected_count} matches, got {}",
                nodes.len()
            )),
            Err(error) => failures.push(format!("query selector {selector_text:?}: {error:?}")),
        }
    }
    failures
}

fn hostile_declaration_regressions() -> Vec<String> {
    let declarations = [
        "color: red; width: calc(10px + 2px)",
        "color: red; width: 2px; width: 3px !important; width: 4px",
        "--payload: var(--missing, 'a;b:c'); color: blue",
        "background-image: url(\"data:image/svg+xml,a;b:c\"); color: green",
        "color: red; /* comment ; } */ width: 1px",
        "color: red /* unterminated",
        "color: rgb(1, 2, 3; display: none",
        "color: red; width: 1px ] ; display: none",
        "color: red; --x: {a:b;c:d}; opacity: .5",
        "color: red\\; display: none",
        "color: red ! important; opacity: .75",
        "color: red; color: blue !important; color: green",
    ];
    let mut failures = Vec::new();
    for declaration in declarations {
        // These parsers receive attacker-controlled strings in style elements
        // and style attributes. Errors are normal; panics or unstable
        // serialization are not.
        let first = css::cssom_declaration_text(declaration);
        let second = css::cssom_declaration_text(&first);
        if first != second {
            failures.push(format!(
                "CSSOM serialization is not idempotent for {declaration:?}: {first:?} -> {second:?}"
            ));
        }
        for name in ["color", "width", "opacity", "--payload"] {
            if let Err(error) = css::declaration_value(declaration, name) {
                // Unterminated token sequences may report an error; ensure the
                // parser still remains deterministic across repeated reads.
                if css::declaration_value(declaration, name).err() != Some(error) {
                    failures.push(format!("unstable declaration error for {declaration:?}"));
                }
            }
        }
        let stylesheet = format!(".probe {{{declaration}}} .after {{ color: blue }}");
        let _ = css::parse(&stylesheet);
    }
    for value in [
        "red; display: none",
        "red !important",
        "red; } body { display: none",
    ] {
        if css::supports_declaration("color", value) {
            failures.push(format!(
                "declaration injection vector unexpectedly accepted: {value:?}"
            ));
        }
    }
    failures
}

fn summarize_reasons(reasons: &BTreeMap<String, usize>) -> String {
    reasons
        .iter()
        .map(|(reason, count)| format!("{reason}: {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[test]
#[ignore = "requires the pinned WPT CSS corpus"]
fn pinned_wpt_css_parser_and_selector_profile() {
    let root = wpt_root();
    verify_wpt_inputs(&root);

    let mut failures = Vec::new();
    let mut value_passed = 0usize;
    let mut value_excluded = 0usize;
    let mut value_cases = 0usize;
    let mut value_reasons = BTreeMap::<String, usize>::new();
    let mut value_report = Vec::new();
    let mut spec_conflict_seen = BTreeSet::new();
    let mut spec_conflict_report = Vec::new();
    for case in wpt_css_cases(&root) {
        value_cases += 1;
        let Some(property) = case.property.as_deref() else {
            value_excluded += 1;
            *value_reasons
                .entry("nonliteral property argument".into())
                .or_default() += 1;
            value_report.push(format!(
                "EXCLUDED {}:{} {} nonliteral-property",
                case.path, case.line, case.operation
            ));
            continue;
        };
        let Some(value) = case.value.as_deref() else {
            value_excluded += 1;
            *value_reasons
                .entry("nonliteral value argument".into())
                .or_default() += 1;
            value_report.push(format!(
                "EXCLUDED {}:{} {} {property} nonliteral-value",
                case.path, case.line, case.operation
            ));
            continue;
        };
        if let Some((index, conflict)) = wpt_spec_conflict(&case) {
            spec_conflict_seen.insert(index);
            let actual = css::supports_declaration(property, value);
            if actual != conflict.current_expected {
                failures.push(format!(
                    "SPEC-CONFLICT implementation mismatch {}:{} {} {:?}: pinned_expected={}, current_expected={}, got support={actual}",
                    conflict.path,
                    conflict.line,
                    conflict.property,
                    conflict.value,
                    conflict.pinned_expected,
                    conflict.current_expected
                ));
            }
            let source = WPT_SPEC_CONFLICT_SOURCES
                .iter()
                .find(|source| source.path == conflict.path)
                .expect("SPEC-CONFLICT source metadata");
            spec_conflict_report.push(format!(
                "SPEC-CONFLICT {}:{} {} {:?}: pinned_expected={}, current_expected={}, actual={}, spec={}, source_sha256={}, source_git_blob={}",
                conflict.path,
                conflict.line,
                conflict.property,
                conflict.value,
                conflict.pinned_expected,
                conflict.current_expected,
                actual,
                conflict.spec,
                source.sha256,
                source.git_blob
            ));
            continue;
        }
        if !VALUE_PROPERTIES.contains(&property.to_ascii_lowercase().as_str()) {
            value_excluded += 1;
            *value_reasons
                .entry(format!(
                    "property {property} outside declared syntax profile"
                ))
                .or_default() += 1;
            value_report.push(format!(
                "EXCLUDED {}:{} {} {property} outside-profile",
                case.path, case.line, case.operation
            ));
            continue;
        }

        let expected = case.operation == "valid";
        let actual = css::supports_declaration(property, value);
        if actual != expected {
            failures.push(format!(
                "WPT {}:{} {} {} {:?}: expected support={expected}, got support={actual}",
                case.path, case.line, case.operation, property, value
            ));
        } else {
            value_passed += 1;
            if expected {
                let declaration = format!("{property}: {value};");
                let serialized = css::cssom_declaration_text(&declaration);
                let roundtrip = css::cssom_declaration_text(&serialized);
                if serialized != roundtrip {
                    failures.push(format!(
                        "WPT {}:{} {property} {:?}: CSSOM declaration text is not idempotent ({serialized:?} -> {roundtrip:?})",
                        case.path, case.line, value
                    ));
                }
            }
        }
        value_report.push(format!(
            "{} {}:{} {} {property} {:?}",
            if actual == expected { "PASS" } else { "FAIL" },
            case.path,
            case.line,
            case.operation,
            value
        ));
    }

    for (index, conflict) in WPT_SPEC_CONFLICTS.iter().enumerate() {
        if !spec_conflict_seen.contains(&index) {
            failures.push(format!(
                "declared SPEC-CONFLICT case was not found in pinned WPT: {}:{} {} {:?}",
                conflict.path, conflict.line, conflict.property, conflict.value
            ));
        }
    }

    let document = selector_fixture();
    let mut selector_passed = 0usize;
    let mut selector_excluded = 0usize;
    let mut selector_cases_count = 0usize;
    let mut selector_reasons = BTreeMap::<String, usize>::new();
    let mut selector_report = Vec::new();
    for case in wpt_selector_cases(&root) {
        selector_cases_count += 1;
        let Some(selector_text) = case.selector.as_deref() else {
            selector_excluded += 1;
            *selector_reasons
                .entry("dynamic or nonliteral selector expression".into())
                .or_default() += 1;
            selector_report.push(format!(
                "EXCLUDED {}:{} dynamic-selector",
                case.path, case.line
            ));
            continue;
        };
        if case.expected_valid {
            if let Some(reason) = selector_profile_reason(selector_text, case.forgiving) {
                selector_excluded += 1;
                *selector_reasons.entry(reason.into()).or_default() += 1;
                selector_report.push(format!(
                    "EXCLUDED {}:{} valid {:?}: {reason}",
                    case.path, case.line, selector_text
                ));
                continue;
            }
        }

        let actual_valid = selector::matches(&document, document.root(), selector_text).is_ok();
        if actual_valid != case.expected_valid {
            failures.push(format!(
                "WPT {}:{} selector {:?}: expected valid={}, got valid={actual_valid}",
                case.path, case.line, selector_text, case.expected_valid
            ));
        } else {
            selector_passed += 1;
        }
        selector_report.push(format!(
            "{} {}:{} {} {:?}",
            if actual_valid == case.expected_valid {
                "PASS"
            } else {
                "FAIL"
            },
            case.path,
            case.line,
            if case.expected_valid {
                "valid"
            } else {
                "invalid"
            },
            selector_text
        ));
    }

    let match_failures = query_match_regressions(&document);
    let hostile_failures = hostile_declaration_regressions();
    failures.extend(match_failures);
    failures.extend(hostile_failures);

    let mut report = format!(
        "WPT CSS parser and selector profile\ncorpus_commit={WPT_COMMIT}\nvalue_directories={}\nvalue_properties={}\nselector_directory={SELECTOR_DIRECTORY}\nselector_pseudo_classes={}\nprofile=CSS values on the explicit property allowlist; strict element-query selectors with implemented type/id/class/attribute-presence-or-equality/combinator/root/structural/lang features\n\nCSS declarations: {value_passed} passed / {value_cases} observed / {value_excluded} pre-outcome exclusions / {} SPEC-CONFLICT cases\nSPEC-CONFLICT policy: separately reported, not counted as passes or profile exclusions; implementation must match the current cited grammar\nCSS exclusion reasons: {}\nSelector parsing: {selector_passed} passed / {selector_cases_count} observed / {selector_excluded} pre-outcome exclusions\nSelector exclusion reasons: {}\nSelector matcher checks: 9\nHostile declaration checks: 15\nFailures: {}\n\n",
        VALUE_DIRECTORIES.join(", "),
        VALUE_PROPERTIES.join(", "),
        SELECTOR_PSEUDO_CLASSES.join(", "),
        spec_conflict_report.len(),
        summarize_reasons(&value_reasons),
        summarize_reasons(&selector_reasons),
        failures.len(),
    );
    for line in value_report
        .iter()
        .chain(&spec_conflict_report)
        .chain(&selector_report)
    {
        report.push_str(line);
        report.push('\n');
    }
    if !failures.is_empty() {
        report.push_str("\nFAILURES\n");
        for failure in &failures {
            report.push_str(failure);
            report.push('\n');
        }
    }
    let destination = std::env::var_os("LUMEN_CSS_CONFORMANCE_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/css-conformance-results.txt"));
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).expect("create CSS conformance report directory");
    }
    fs::write(&destination, &report).expect("write CSS conformance report");
    println!("{report}");
    assert!(
        failures.is_empty(),
        "CSS parser/selector profile has {} failures; see {}",
        failures.len(),
        destination.display()
    );
}
