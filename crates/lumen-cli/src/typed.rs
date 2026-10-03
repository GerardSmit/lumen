//! `lumen typed`: dumps the typed-tier facts for TypeScript / JSDoc files.
//!
//! ```text
//! lumen typed [--report | --json | --facts | --strip] [--tsconfig PATH] [--lang ts|js] FILE...
//! ```
//!
//! - `--report` (default): the `--typed-report` text (docs/typed-tier.md §4.7).
//! - `--json`: the whole table as JSON (`--typed-report=json`).
//! - `--facts`: every fact with the source text it is keyed to.
//! - `--strip`: the location-preserving stripped JavaScript.
//! - `--strip-cases`: FILE holds snippets separated by `\n=====\n`; prints one line per
//!   snippet: the stripped text as a JSON string, or `ERR <code> <message>` (for differential
//!   tests against `node:module`'s `stripTypeScriptTypes`).

use std::path::{Path, PathBuf};

use lumen::typescript::strip_types;
#[cfg(feature = "typed")]
use lumen::typescript::{analyze, dump_facts, report_json, report_text, AnalyzeOptions, Lang};
#[cfg(feature = "typed")]
use lumen_runtime::tsconfig;

#[cfg(not(feature = "typed"))]
#[derive(Clone, Copy)]
enum Lang {
    Ts,
    Js,
}

fn usage() -> i32 {
    eprintln!(
        "usage: lumen typed [--report | --json | --facts | --strip | --strip-cases] [--tsconfig PATH] [--lang ts|js] FILE..."
    );
    2
}

/// `lumen typed ARGS...` (the arguments after `typed`).
pub fn main(argv: Vec<String>) -> i32 {
    let mut mode = "report";
    let mut tsconfig_path: Option<PathBuf> = None;
    let mut lang: Option<Lang> = None;
    let mut files = Vec::new();
    let mut args = argv.into_iter();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--report" => mode = "report",
            "--json" => mode = "json",
            "--facts" => mode = "facts",
            "--strip" => mode = "strip",
            "--strip-cases" => mode = "strip-cases",
            "--tsconfig" => match args.next() {
                Some(p) => tsconfig_path = Some(p.into()),
                None => return usage(),
            },
            "--lang" => match args.next().as_deref() {
                Some("ts") => lang = Some(Lang::Ts),
                Some("js") => lang = Some(Lang::Js),
                _ => return usage(),
            },
            "-h" | "--help" => return usage(),
            s if s.starts_with('-') => return usage(),
            _ => files.push(PathBuf::from(a)),
        }
    }
    if files.is_empty() {
        return usage();
    }
    let mut status = 0;
    for file in &files {
        if let Err(e) = run(file, mode, tsconfig_path.as_deref(), lang) {
            eprintln!("{e}");
            status = 1;
        }
    }
    status
}

fn run(
    file: &Path,
    mode: &str,
    tsconfig_path: Option<&Path>,
    lang: Option<Lang>,
) -> Result<(), String> {
    let src = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let name = file.display().to_string();
    if mode == "strip-cases" {
        let src = src.replace("\r\n", "\n");
        for case in src.split("\n=====\n") {
            match strip_types(case) {
                Ok(out) => println!("{}", lumen_common::json::json_string(&out)),
                Err(e) => println!(
                    "ERR {} {} @{}:{}",
                    e.code.unwrap_or("SyntaxError"),
                    e.message,
                    e.line,
                    e.column
                ),
            }
        }
        return Ok(());
    }
    if mode == "strip" {
        let out = strip_types(&src).map_err(|e| format!("{name}:{}:{}: {e}", e.line, e.column))?;
        use std::io::Write;
        let _ = std::io::stdout().write_all(out.as_bytes());
        return Ok(());
    }
    analyze_mode(file, &src, &name, mode, tsconfig_path, lang)
}

#[cfg(not(feature = "typed"))]
fn analyze_mode(
    _file: &Path,
    _src: &str,
    _name: &str,
    _mode: &str,
    _tsconfig_path: Option<&Path>,
    _lang: Option<Lang>,
) -> Result<(), String> {
    let _ = (Lang::Ts, Lang::Js);
    Err("this lumen was built without the `typed` feature: only --strip and --strip-cases are available".into())
}

#[cfg(feature = "typed")]
fn analyze_mode(
    file: &Path,
    src: &str,
    name: &str,
    mode: &str,
    tsconfig_path: Option<&Path>,
    lang: Option<Lang>,
) -> Result<(), String> {
    let compiler = match tsconfig_path {
        Some(p) => tsconfig::load(p)?,
        None => tsconfig::load_for(file)?,
    };
    let lang = lang.or_else(|| Lang::from_path(file)).unwrap_or(Lang::Ts);
    let a = analyze(src, &AnalyzeOptions { lang, compiler }).map_err(|d| format!("{name}:{d}"))?;
    match mode {
        "json" => print!("{}", report_json(&a.table, src, name)),
        "facts" => print!("{}", dump_facts(&a.table, src)),
        _ => print!("{}", report_text(&a.table, src, name)),
    }
    Ok(())
}

