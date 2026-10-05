//! Pinned Babel and TypeScript JSX parser fixtures plus evaluated transform parity.
use lumen::{Completion, Engine, JsxOptions, JsxRuntime, transpile_jsx};
use lumen_common::json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus_root() -> PathBuf {
    std::env::var_os("LUMEN_JSX_CORPUS_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(4)
                .expect("Lumen crate must be under the repository's external/lumen/crates tree")
                .join("target/corpora")
        })
}

fn git_output(repository: &Path, arguments: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .unwrap_or_else(|error| panic!("cannot run git for {}: {error}", repository.display()));
    assert!(
        output.status.success(),
        "git {:?} failed in {}: {}",
        arguments,
        repository.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output is UTF-8")
}

fn assert_pinned_clean_repository(
    repository: &Path,
    expected_commit: &str,
    expected_tag: &str,
    input_paths: &[&str],
    report: &mut String,
) {
    let head = git_output(repository, &["rev-parse", "HEAD"]);
    assert_eq!(
        head.trim(),
        expected_commit,
        "unexpected upstream pin in {}",
        repository.display()
    );
    let tag = git_output(repository, &["describe", "--tags", "--exact-match", "HEAD"]);
    assert_eq!(
        tag.trim(),
        expected_tag,
        "unexpected upstream tag in {}",
        repository.display()
    );

    let mut status_arguments = vec![
        "status",
        "--porcelain",
        "--untracked-files=all",
        "--ignored=matching",
        "--",
    ];
    status_arguments.extend_from_slice(input_paths);
    let status = git_output(repository, &status_arguments);
    assert!(
        status.is_empty(),
        "profile inputs changed from the pinned upstream tree in {}:\n{status}",
        repository.display()
    );
    writeln!(
        report,
        "PIN {} {expected_tag} {expected_commit}: profile inputs clean",
        repository.file_name().unwrap_or_default().to_string_lossy()
    )
    .unwrap();
}

fn collect_named(directory: &Path, file_name: &str, output: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()));
    for entry in entries {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_named(&path, file_name, output);
        } else if path.file_name().is_some_and(|name| name == file_name) {
            output.push(path);
        }
    }
}

fn fixture_options(path: &Path) -> Option<Value> {
    let mut directory = path.parent()?;
    loop {
        let options = directory.join("options.json");
        if let Ok(source) = std::fs::read_to_string(options) {
            return Some(lumen_common::json::parse(&source).unwrap());
        }
        directory = directory.parent()?;
    }
}

fn babel_profile_exclusion(
    _path: &Path,
    options: Option<&Value>,
    source: &str,
) -> Option<&'static str> {
    let has_flow = matches!(
        options.and_then(|value| value.get("plugins")),
        Some(Value::Arr(plugins)) if plugins.iter().filter_map(Value::as_str).any(|plugin| plugin == "flow")
    );
    if has_flow && flow_generic_arrow_ambiguity(source) {
        return Some(
            "Flow generic-arrow syntax is outside the ECMAScript/TypeScript JSX parser profile",
        );
    }
    if let Some(Value::Arr(plugins)) = options.and_then(|value| value.get("plugins")) {
        let has_jsx = plugins
            .iter()
            .filter_map(Value::as_str)
            .any(|plugin| plugin == "jsx");
        if !has_jsx {
            return Some(
                "fixture disables the JSX parser plugin; this profile exercises the JSX-enabled entry point",
            );
        }
    }
    if options
        .and_then(|value| value.get("sourceType"))
        .and_then(Value::as_str)
        == Some("script")
    {
        return Some("script-mode parser fixtures are outside the module-only entry point");
    }
    if matches!(
        options.and_then(|value| value.get("errorRecovery")),
        Some(Value::Bool(true))
    ) {
        return Some(
            "Babel error-recovery mode returns an AST with diagnostics; the strict JSX entry point rejects malformed input",
        );
    }
    None
}

fn flow_generic_arrow_ambiguity(source: &str) -> bool {
    let source = source.trim_start();
    let Some(after_open) = source.strip_prefix('<') else {
        return false;
    };
    let name_len = after_open
        .char_indices()
        .take_while(|(_, character)| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '$')
        })
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    if name_len == 0 {
        return false;
    }
    let Some(close) = after_open.find('>') else {
        return false;
    };
    if close < name_len {
        return false;
    }
    let rest = after_open[close + 1..].trim_start();
    rest.starts_with("() =>") || (rest.starts_with('(') && rest.contains(") =>"))
}

fn babel_uses_typescript(options: Option<&Value>) -> bool {
    matches!(
        options.and_then(|value| value.get("plugins")),
        Some(Value::Arr(plugins)) if plugins.iter().filter_map(Value::as_str).any(|plugin| plugin == "typescript")
    )
}

fn run_parser_corpus(
    paths: &[PathBuf],
    base: &Path,
    expected_rejection: impl Fn(&Path, &str) -> bool,
    mode: impl Fn(&Path, Option<&Value>) -> bool,
    exclusion: impl Fn(&Path, Option<&Value>, &str) -> Option<&'static str>,
    options_for: impl Fn() -> JsxOptions,
    report: &mut String,
) -> (usize, usize, usize, usize) {
    let (mut accepted, mut rejected, mut excluded, mut failed) = (0, 0, 0, 0);
    for path in paths {
        let identity = path
            .strip_prefix(base)
            .unwrap_or(path)
            .display()
            .to_string();
        let options = fixture_options(path);
        let source = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("cannot read {identity}: {error}"));
        if let Some(reason) = exclusion(path, options.as_ref(), &source) {
            excluded += 1;
            writeln!(report, "EXCLUDED {identity}: {reason}").unwrap();
            continue;
        }
        let should_reject = expected_rejection(path, &source);
        let typescript = mode(path, options.as_ref());
        let jsx_options = options_for();
        let result = if typescript {
            parse_typescript_fixture(&source, &jsx_options)
        } else {
            transpile_jsx(&source, false, &jsx_options)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        };
        match (should_reject, result) {
            (false, Ok(_)) => {
                accepted += 1;
                writeln!(report, "PASS {identity}: accepted").unwrap();
            }
            (true, Err(_)) => {
                rejected += 1;
                writeln!(report, "PASS {identity}: rejected").unwrap();
            }
            (false, Err(error)) => {
                failed += 1;
                writeln!(report, "FAIL {identity}: unexpectedly rejected: {error:?}").unwrap();
            }
            (true, Ok(_)) => {
                failed += 1;
                writeln!(report, "FAIL {identity}: unexpectedly accepted").unwrap();
            }
        }
    }
    (accepted, rejected, excluded, failed)
}

fn parse_typescript_fixture(source: &str, options: &JsxOptions) -> Result<(), String> {
    for (index, file) in typescript_source_files(source).into_iter().enumerate() {
        transpile_jsx(&file, true, options).map_err(|error| {
            format!(
                "virtual TSX file {}: {error:?}; source: {}",
                index + 1,
                file.trim()
            )
        })?;
    }
    Ok(())
}

/// TypeScript's conformance cases can be virtual multi-file programs. Parse each TSX source on
/// its own; declarations in companion files are type-checker setup, not part of JSX grammar.
fn typescript_source_files(source: &str) -> Vec<String> {
    let mut files = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_source = String::new();
    let mut found_directive = false;
    for line in source.split_inclusive('\n') {
        let directive = line
            .trim_start()
            .strip_prefix("//")
            .map(str::trim_start)
            .and_then(|line| line.strip_prefix("@filename:"))
            .map(str::trim);
        if let Some(name) = directive {
            found_directive = true;
            if current_name
                .as_deref()
                .is_some_and(|name| name.to_ascii_lowercase().ends_with(".tsx"))
            {
                files.push(std::mem::take(&mut current_source));
            }
            current_name = Some(name.to_string());
            continue;
        }
        if current_name
            .as_deref()
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".tsx"))
        {
            current_source.push_str(line);
        }
    }
    if current_name
        .as_deref()
        .is_some_and(|name| name.to_ascii_lowercase().ends_with(".tsx"))
    {
        files.push(current_source);
    }
    if !found_directive || files.is_empty() {
        vec![source.to_string()]
    } else {
        files
    }
}

fn tsx_profile_exclusion(
    _path: &Path,
    _options: Option<&Value>,
    source: &str,
) -> Option<&'static str> {
    let tsx_files = typescript_source_files(source);
    let tokens = ts_tokens(&tsx_files.join("\n"));
    for (index, token) in tokens.iter().enumerate() {
        if token == "import" {
            let mut next = index + 1;
            if tokens.get(next).is_some_and(|token| token == "type") {
                next += 1;
            }
            if tokens.get(next).is_some_and(|token| is_identifier(token))
                && tokens.get(next + 1).is_some_and(|token| token == "=")
            {
                return Some(
                    "TypeScript import-equals syntax is outside the parser's strip-only TSX grammar",
                );
            }
        }
        if token == "namespace"
            && tokens
                .get(index + 1)
                .is_some_and(|next| is_identifier(next))
        {
            return Some(
                "TypeScript namespace declarations are non-erasable and outside the strip-only TSX grammar",
            );
        }
        if token == "module"
            && index > 0
            && matches!(tokens[index - 1].as_str(), "declare" | "export")
        {
            return Some(
                "TypeScript ambient module declarations are outside the strip-only TSX grammar",
            );
        }
        if token == "enum"
            && tokens
                .get(index + 1)
                .is_some_and(|next| is_identifier(next))
        {
            return Some(
                "TypeScript enum declarations are non-erasable and outside the strip-only TSX grammar",
            );
        }
        if token == "constructor" {
            let mut depth = 0usize;
            for parameter in tokens.iter().skip(index + 1) {
                match parameter.as_str() {
                    "(" => depth += 1,
                    ")" if depth == 0 => break,
                    ")" => depth -= 1,
                    "public" | "private" | "protected" | "readonly" if depth == 1 => {
                        return Some(
                            "TypeScript parameter properties are non-erasable and outside the strip-only TSX grammar",
                        );
                    }
                    _ => {}
                }
            }
        }
    }
    None
}

fn is_identifier(token: &str) -> bool {
    token
        .chars()
        .next()
        .is_some_and(|first| first == '$' || first == '_' || first.is_ascii_alphabetic())
}

/// Small lexical pass for predeclared TS profile exclusions. It ignores comments and string
/// literals so ordinary prose and module specifiers cannot accidentally change fixture scope.
fn ts_tokens(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
        } else if bytes[index..].starts_with(b"/*") {
            index += 2;
            while index + 1 < bytes.len() && &bytes[index..index + 2] != b"*/" {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
        } else if matches!(bytes[index], b'\'' | b'"' | b'`') {
            let quote = bytes[index];
            index += 1;
            let mut escaped = false;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == quote {
                    break;
                }
            }
        } else if bytes[index].is_ascii_alphabetic() || matches!(bytes[index], b'_' | b'$') {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
            {
                index += 1;
            }
            tokens.push(source[start..index].to_string());
        } else {
            if !bytes[index].is_ascii_whitespace() {
                tokens.push((bytes[index] as char).to_string());
            }
            index += 1;
        }
    }
    tokens
}

fn ts_expected_parse_error(path: &Path, baseline_root: &Path) -> bool {
    const JSX_PARSE_DIAGNOSTICS: &[&str] = &[
        "TS1003", "TS1005", "TS1099", "TS1109", "TS1381", "TS1382", "TS17002", "TS17008",
        "TS17014", "TS17015", "TS18007", "TS2657",
    ];
    let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(baseline_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        let Some(suffix) = name.strip_prefix(stem) else {
            return false;
        };
        if !(suffix.starts_with('.') || suffix.starts_with('(')) || !name.ends_with(".errors.txt") {
            return false;
        }
        std::fs::read_to_string(entry.path())
            .ok()
            .is_some_and(|baseline| {
                JSX_PARSE_DIAGNOSTICS
                    .iter()
                    .any(|code| baseline.contains(code))
            })
    })
}

fn babel_expected_parse_error(path: &Path, options: Option<&Value>) -> bool {
    if options
        .and_then(|options| options.get("throws"))
        .and_then(Value::as_str)
        .is_some()
    {
        return true;
    }
    let output_path = path.parent().unwrap().join("output.json");
    if !output_path.exists() {
        return true;
    }
    let source = std::fs::read_to_string(output_path).unwrap();
    let output = lumen_common::json::parse(&source).unwrap();
    matches!(output.get("errors"), Some(Value::Arr(errors)) if !errors.is_empty())
}

fn react_runtime() -> &'static str {
    "globalThis.React={Fragment:'Fragment',createElement(type,props,...children){return {type,props,children}},jsx(type,props,key){return {type,props,key,children:Object.prototype.hasOwnProperty.call(props,'children')?[props.children]:[]}},jsxs(type,props,key){return {type,props,key,children:Object.prototype.hasOwnProperty.call(props,'children')?[props.children]:[]}}};globalThis.require=(name)=>name.endsWith('/jsx-runtime')?{Fragment:React.Fragment,jsx:React.jsx,jsxs:React.jsxs}:{createElement:React.createElement};globalThis.dom=(...args)=>React.createElement(...args);globalThis.dom.createElement=globalThis.dom;globalThis.DomFrag='Fragment';globalThis.prop='value';"
}

fn split_leading_comments(source: &str) -> (&str, &str) {
    let mut rest = source;
    loop {
        let whitespace = rest.len() - rest.trim_start().len();
        rest = &rest[whitespace..];
        if rest.starts_with("//") {
            let end = rest.find('\n').unwrap_or(rest.len());
            rest = &rest[end..];
        } else if rest.starts_with("/*") {
            let Some(end) = rest.find("*/") else {
                return (source, "");
            };
            rest = &rest[end + 2..];
        } else {
            let prefix_len = source.len() - rest.len();
            return (&source[..prefix_len], rest);
        }
    }
}

fn instrument_program(source: &str, bind_result: &str, expression_list: bool) -> String {
    if !expression_list {
        return format!("{source}\nglobalThis.__jsxResult = {bind_result};");
    }
    let (comments, code) = split_leading_comments(source);
    let expressions = code
        .split(';')
        .map(str::trim)
        .filter(|expression| !expression.is_empty())
        .collect::<Vec<_>>()
        .join(",");
    format!("{comments}\nglobalThis.__jsxResult = [{expressions}];")
}

fn rewrite_runtime_imports(source: &str) -> String {
    let mut output = String::with_capacity(source.len() + 128);
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("import * as ") {
            if let Some((local, module)) = rest.split_once(" from ") {
                let module = module.trim().trim_end_matches(';');
                writeln!(output, "const {local} = require({module});").unwrap();
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix("import {") {
            if let Some((specifiers, module)) = rest.split_once("} from ") {
                let bindings = specifiers
                    .split(',')
                    .map(str::trim)
                    .filter(|specifier| !specifier.is_empty())
                    .map(|specifier| match specifier.split_once(" as ") {
                        Some((imported, local)) => format!("{imported}: {local}"),
                        None => specifier.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let module = module.trim().trim_end_matches(';');
                writeln!(output, "const {{{bindings}}} = require({module});").unwrap();
                continue;
            }
        }
        output.push_str(line);
        output.push('\n');
    }
    output
}

fn evaluate_transform_pair(
    source: &str,
    babel_output: &str,
    source_key: &str,
    bind_result: &str,
    expression_list: bool,
    options: JsxOptions,
) -> Result<(), String> {
    let wrapped = instrument_program(source, bind_result, expression_list);

    let mut native = Engine::new();
    native
        .eval(react_runtime(), false)
        .map_err(|error| format!("runtime setup: {error:?}"))?;
    native.set_jsx_options(options.clone());
    match native
        .eval_module_jsx(&wrapped, source_key, false, |specifier, _| {
            matches!(specifier, "react/jsx-runtime" | "react/jsx-dev-runtime").then(|| {
                (
                    specifier.to_string(),
                    "export const jsx=React.jsx; export const jsxs=React.jsxs; export const Fragment=React.Fragment;".to_string(),
                )
            })
        })
        .map_err(|error| format!("native JSX evaluation: {error:?}"))?
    {
        Completion::Throw { message, .. } => return Err(format!("native JSX threw: {message}")),
        Completion::Value(_) => {}
    }
    let native_result = match native
        .eval("JSON.stringify(globalThis.__jsxResult)", false)
        .map_err(|error| format!("read native result: {error:?}"))?
    {
        Completion::Value(value) => value,
        Completion::Throw { message, .. } => {
            return Err(format!("read native result threw: {message}"));
        }
    };

    let transformed = transpile_jsx(&wrapped, false, &options)
        .map_err(|error| format!("JSX transpilation: {error:?}"))?;
    let mut printed = Engine::new();
    printed
        .eval(react_runtime(), false)
        .map_err(|error| format!("runtime setup: {error:?}"))?;
    match printed
        .eval(&rewrite_runtime_imports(&transformed), false)
        .map_err(|error| format!("transformed output evaluation: {error:?}"))?
    {
        Completion::Throw { message, .. } => {
            return Err(format!("transformed output threw: {message}"));
        }
        Completion::Value(_) => {}
    }
    let transformed_result = match printed
        .eval("JSON.stringify(globalThis.__jsxResult)", false)
        .map_err(|error| format!("read transformed result: {error:?}"))?
    {
        Completion::Value(value) => value,
        Completion::Throw { message, .. } => {
            return Err(format!("read transformed result threw: {message}"));
        }
    };
    let expected = instrument_program(babel_output, bind_result, expression_list);
    let mut babel = Engine::new();
    babel
        .eval(react_runtime(), false)
        .map_err(|error| format!("Babel runtime setup: {error:?}"))?;
    match babel
        .eval(&rewrite_runtime_imports(&expected), false)
        .map_err(|error| format!("Babel output evaluation: {error:?}"))?
    {
        Completion::Throw { message, .. } => {
            return Err(format!("Babel output threw: {message}"));
        }
        Completion::Value(_) => {}
    }
    let babel_result = match babel
        .eval("JSON.stringify(globalThis.__jsxResult)", false)
        .map_err(|error| format!("read Babel result: {error:?}"))?
    {
        Completion::Value(value) => value,
        Completion::Throw { message, .. } => {
            return Err(format!("read Babel result threw: {message}"));
        }
    };
    if native_result != transformed_result || native_result != babel_result {
        return Err(format!(
            "native={native_result}; transpiled={transformed_result}; Babel={babel_result}"
        ));
    }
    Ok(())
}

fn run_upstream_transform_pairs(corpus: &Path, report: &mut String) -> (usize, usize) {
    // These pinned Babel transform fixtures cover nested nodes, fragment keys,
    // explicit children, duplicate props, and pragma-selected factories. Each
    // source is evaluated both through the native module path and as emitted JS.
    let fixtures: &[(&str, &str, bool, JsxRuntime)] = &[
        ("runtime/classic", "x", false, JsxRuntime::Classic),
        (
            "react-automatic/handle-static-children",
            "x",
            false,
            JsxRuntime::Automatic,
        ),
        (
            "react-automatic/handle-fragments",
            "x",
            false,
            JsxRuntime::Automatic,
        ),
        ("react/duplicate-props", "", true, JsxRuntime::Classic),
        (
            "react/should-allow-pragmafrag-and-frag",
            "",
            true,
            JsxRuntime::Classic,
        ),
        (
            "pure/true-default-pragma-classic-runtime",
            "",
            true,
            JsxRuntime::Classic,
        ),
        (
            "pure/unset-default-pragma-classic-runtime",
            "",
            true,
            JsxRuntime::Classic,
        ),
    ];
    let mut passed = 0;
    let mut failed = 0;
    for (fixture, binding, expression_list, runtime) in fixtures {
        let directory = corpus
            .join("packages/babel-plugin-transform-react-jsx/test/fixtures")
            .join(fixture);
        let input_path = directory.join("input.js");
        let source = std::fs::read_to_string(&input_path).unwrap_or_else(|error| {
            panic!(
                "cannot read upstream transform fixture {}: {error}",
                input_path.display()
            )
        });
        let output_path = [directory.join("output.js"), directory.join("output.mjs")]
            .into_iter()
            .find(|path| path.exists())
            .unwrap_or_else(|| panic!("missing Babel output fixture for {}", input_path.display()));
        let babel_output = std::fs::read_to_string(&output_path).unwrap_or_else(|error| {
            panic!(
                "cannot read Babel output {}: {error}",
                output_path.display()
            )
        });
        let identity = fixture;
        let options = JsxOptions {
            runtime: *runtime,
            ..JsxOptions::default()
        };
        match evaluate_transform_pair(
            &source,
            &babel_output,
            identity,
            binding,
            *expression_list,
            options,
        ) {
            Ok(()) => {
                passed += 1;
                writeln!(report, "TRANSFORM-PASS {identity}").unwrap();
            }
            Err(error) => {
                failed += 1;
                writeln!(report, "TRANSFORM-FAIL {identity}: {error}").unwrap();
            }
        }
    }
    (passed, failed)
}

#[test]
#[ignore = "requires the pinned Babel and TypeScript source corpora"]
fn babel_typescript_jsx_upstream_profile() {
    let corpora = corpus_root();
    let babel = corpora.join("babel/packages/babel-parser/test/fixtures/jsx");
    let typescript = corpora.join("typescript/tests/cases/conformance/jsx");
    let mut babel_paths = Vec::new();
    collect_named(&babel, "input.js", &mut babel_paths);
    babel_paths.sort();
    let mut typescript_paths = Vec::new();
    // The TypeScript corpus stores each test as its source filename, so collect
    // recursively and keep only the grammar-conformance TSX inputs.
    fn collect_tsx(directory: &Path, output: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_tsx(&path, output);
            } else if path.extension().is_some_and(|extension| extension == "tsx") {
                output.push(path);
            }
        }
    }
    collect_tsx(&typescript, &mut typescript_paths);
    typescript_paths.sort();
    assert!(!babel_paths.is_empty(), "missing Babel JSX parser fixtures");
    assert!(
        !typescript_paths.is_empty(),
        "missing TypeScript TSX fixtures"
    );

    let mut report = String::new();
    assert_pinned_clean_repository(
        &corpora.join("babel"),
        "4fba7541180bf5f58256d8e358b544e3831ad090",
        "v7.29.7",
        &[
            "packages/babel-parser/test/fixtures/jsx",
            "packages/babel-plugin-transform-react-jsx/test/fixtures",
        ],
        &mut report,
    );
    assert_pinned_clean_repository(
        &corpora.join("typescript"),
        "c63de15a992d37f0d6cec03ac7631872838602cb",
        "v5.9.3",
        &["tests/cases/conformance/jsx", "tests/baselines/reference"],
        &mut report,
    );
    let babel_counts = run_parser_corpus(
        &babel_paths,
        &babel,
        |path, _| babel_expected_parse_error(path, fixture_options(path).as_ref()),
        |_, options| babel_uses_typescript(options),
        babel_profile_exclusion,
        JsxOptions::default,
        &mut report,
    );
    let typescript_baselines = corpora.join("typescript/tests/baselines/reference");
    let typescript_counts = run_parser_corpus(
        &typescript_paths,
        &typescript,
        |path, _| ts_expected_parse_error(path, &typescript_baselines),
        |_, _| true,
        tsx_profile_exclusion,
        JsxOptions::default,
        &mut report,
    );
    let (transform_passed, transform_failed) =
        run_upstream_transform_pairs(&corpora.join("babel"), &mut report);
    let report_path = std::env::var_os("LUMEN_JSX_REPORT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            corpora
                .parent()
                .expect("target/corpora has target parent")
                .join("html-jsx-results.txt")
        });
    std::fs::write(&report_path, &report).unwrap();
    let failed = babel_counts.3 + typescript_counts.3 + transform_failed;
    eprintln!(
        "JSX profile: Babel accepted={} rejected={} excluded={}; TypeScript accepted={} rejected={} excluded={}; evaluated transforms={transform_passed} passed, {transform_failed} failed",
        babel_counts.0,
        babel_counts.1,
        babel_counts.2,
        typescript_counts.0,
        typescript_counts.1,
        typescript_counts.2,
    );
    assert_eq!(failed, 0, "see {}", report_path.display());
}
