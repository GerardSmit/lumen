//! Differential tests against Node's type stripper, over `tests/ts_fixtures`:
//!
//! - the Rust stripper's output is byte-identical to Node's `module.stripTypeScriptTypes`
//!   (`X.strip.golden`, including rejections, their error codes and messages);
//! - every function node reaches through the stripped module's exports has its
//!   `Function.prototype.toString()` range (`X.offsets.json`, i.e. the engine's
//!   `FnSource::Range`) as a key of the type table, with the same end.
//!
//! The goldens come from `node tests/ts_gen_goldens.mjs` (run it after changing a fixture).

#![cfg(feature = "typed")]

use std::path::{Path, PathBuf};

use lumen::typescript::{analyze, strip_types, AnalyzeOptions, CompilerOptions, Lang};

fn fixtures() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/ts_fixtures");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("ts" | "js" | "mts" | "mjs")
            )
        })
        .collect();
    out.sort();
    assert!(out.len() >= 5, "fixtures missing in {}", dir.display());
    out
}

fn golden(path: &Path, ext: &str) -> Option<String> {
    let stem = path.file_stem().unwrap().to_str().unwrap();
    std::fs::read_to_string(path.with_file_name(format!("{stem}.{ext}"))).ok()
}

/// Reads git's view of a fixture: normalizes CRLF checkouts so the test does not depend on
/// `core.autocrlf`.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().replace("\r\n", "\n")
}

#[test]
fn strip_matches_node() {
    let mut checked = 0;
    for path in fixtures() {
        let src = read(&path);
        let want = golden(&path, "strip.golden")
            .unwrap_or_else(|| panic!("no golden for {}", path.display()))
            .replace("\r\n", "\n");
        let got = match strip_types(&src) {
            Ok(s) => s,
            Err(e) => format!("ERROR {} {}\n", e.code.unwrap_or("SyntaxError"), e.message),
        };
        if got != want {
            let line = got
                .lines()
                .zip(want.lines())
                .position(|(a, b)| a != b)
                .map(|i| i + 1);
            panic!(
                "{}: strip output differs from Node's (first differing line {line:?})\n--- rust\n{got}\n--- node\n{want}",
                path.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 5);
}

struct Probe {
    path: String,
    start: u32,
    end: u32,
}

/// Parses the generator's fixed JSON layout (an array of `{path, start, end}` objects).
fn parse_offsets(text: &str) -> Vec<Probe> {
    let mut out = Vec::new();
    for obj in text.split('{').skip(1) {
        let field = |name: &str| -> &str {
            let at = obj.find(&format!("\"{name}\":")).expect(name) + name.len() + 3;
            let rest = obj[at..].trim_start();
            if let Some(s) = rest.strip_prefix('"') {
                &s[..s.find('"').unwrap()]
            } else {
                let end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                &rest[..end]
            }
        };
        out.push(Probe {
            path: field("path").to_string(),
            start: field("start").parse().unwrap(),
            end: field("end").parse().unwrap(),
        });
    }
    out
}

#[test]
fn table_keys_match_engine_function_offsets() {
    let mut files = 0;
    let mut probes = 0;
    for path in fixtures() {
        let Some(json) = golden(&path, "offsets.json") else {
            continue;
        };
        let src = read(&path);
        let lang = Lang::from_path(&path).unwrap();
        let a = analyze(
            &src,
            &AnalyzeOptions {
                lang,
                compiler: CompilerOptions::default(),
            },
        )
        .unwrap_or_else(|d| panic!("{}: {d}", path.display()));
        let stripped = strip_types(&src).unwrap();
        for p in parse_offsets(&json) {
            let text = &stripped[p.start as usize..p.end as usize];
            let what = format!("{}: {} `{text}`", path.display(), p.path);
            let f = a.table.fn_at(p.start);
            let c = a.table.class_at(p.start);
            assert!(
                f.is_some() || c.is_some(),
                "{what}: no table entry at {}",
                p.start
            );
            if let Some(f) = f {
                assert_eq!(f.end, p.end, "{what}: function end");
            }
            if let Some((_, c)) = c {
                assert_eq!(c.end, p.end, "{what}: class end");
            }
            probes += 1;
        }
        files += 1;
    }
    assert!(files >= 3 && probes >= 30, "{files} files, {probes} probes");
}

#[test]
fn every_fixture_parses() {
    for path in fixtures() {
        let src = read(&path);
        let lang = Lang::from_path(&path).unwrap();
        analyze(
            &src,
            &AnalyzeOptions {
                lang,
                compiler: CompilerOptions::default(),
            },
        )
        .unwrap_or_else(|d| panic!("{}: {d}", path.display()));
    }
}
