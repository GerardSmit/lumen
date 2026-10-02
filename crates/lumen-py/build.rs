//! Packs the vendored pure-Python standard library (`lib/`) into the binary.
//!
//! Each module is stripped of comments with the interpreter's own tokenizer (line numbers and
//! docstrings stay intact), then the modules are grouped into chunks that are Brotli-compressed
//! and decompressed lazily at run time (`src/frozen.rs`). The sources in `lib/` are never
//! modified.
//!
//! Features: `py-stdlib-full` embeds every module (default drops those listed in
//! `stdlib-exclude.txt` and in `LUMEN_PY_STDLIB_EXCLUDE`), `py-stdlib-comments` keeps comments,
//! `py-stdlib-external` embeds nothing (the library is read from a directory at run time).

#![allow(dead_code)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

#[path = "src/ast.rs"]
mod ast;
#[path = "src/digits.rs"]
mod digits;
#[path = "src/lexer.rs"]
mod lexer;
#[path = "src/syntax_error.rs"]
mod syntax_error;
#[path = "src/unicode.rs"]
mod unicode;

mod parser {
    pub use crate::syntax_error::SyntaxError;
}

mod limits {
    pub use crate::digits::{digit_limit_message, literal_digit_limit};
}

use lexer::Tok;

const CHUNK_TARGET: usize = 256 * 1024;

fn collect(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).expect("read lib dir").filter_map(Result::ok).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let child = if rel.is_empty() { name.clone() } else { format!("{}/{}", rel, name) };
        if path.is_dir() {
            collect(&path, &child, out);
        } else if name.ends_with(".py") {
            out.push((child, path));
        }
    }
}

/// A rule matches a path component when it has no `/`, a path prefix otherwise.
fn excluded(rules: &[String], rel: &str) -> bool {
    rules.iter().any(|rule| {
        if rule.contains('/') {
            let prefix = rule.trim_end_matches('/');
            rel == prefix || rel.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
        } else {
            rel.split('/').any(|part| part == rule)
        }
    })
}

fn parse_rules(text: &str, out: &mut Vec<String>) {
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if !line.is_empty() {
            out.push(line.to_string());
        }
    }
}

type Significant = Vec<(Tok, String, (u32, u32), (u32, u32))>;

fn significant(src: &str) -> Option<Significant> {
    let (toks, err) = lexer::tokenize_extra(src);
    if err.is_some() {
        return None;
    }
    Some(
        toks.into_iter()
            .filter(|t| !matches!(t.tok, Tok::Comment | Tok::Nl))
            .map(|t| match t.tok {
                Tok::Newline => (t.tok, t.text, (t.start.0, 0), (t.end.0, 0)),
                _ => (t.tok, t.text, t.start, t.end),
            })
            .collect(),
    )
}

/// `src` without comments. Newlines stay, so every other token keeps its line; `None` when the
/// source cannot be stripped provably safely (the caller embeds it unchanged).
fn strip_comments(src: &str) -> Option<String> {
    if src.contains(['\r', '\0', '\u{feff}']) {
        return None;
    }
    let before = significant(src)?;
    let (toks, _) = lexer::tokenize_extra(src);
    let mut comments: Vec<(u32, u32, usize)> = Vec::new();
    for t in &toks {
        if t.tok == Tok::Comment {
            if !t.text.starts_with('#') || t.text.contains('\n') {
                return None;
            }
            comments.push((t.start.0, t.start.1, t.text.len()));
        }
    }
    let mut lines: Vec<String> = src.split('\n').map(str::to_string).collect();
    for &(line, col, len) in comments.iter().rev() {
        let l = lines.get_mut(line as usize - 1)?;
        let start = l.char_indices().nth(col as usize).map(|(i, _)| i)?;
        let end = start + len;
        if l.get(start..end)?.as_bytes()[0] != b'#' {
            return None;
        }
        let head = l[..start].trim_end_matches([' ', '\t']).to_string();
        let tail = l[end..].to_string();
        *l = head + &tail;
    }
    let out = lines.join("\n");
    (significant(&out)? == before && out.matches('\n').count() == src.matches('\n').count()).then_some(out)
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let lib = manifest.join("lib");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let feature = |name: &str| std::env::var_os(format!("CARGO_FEATURE_{}", name)).is_some();
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=stdlib-exclude.txt");
    println!("cargo:rerun-if-changed=src/lexer.rs");
    println!("cargo:rerun-if-changed=src/lexer");
    println!("cargo:rerun-if-env-changed=LUMEN_PY_STDLIB_EXCLUDE");
    println!("cargo:rerun-if-changed={}", lib.display());

    let mut generated = String::new();
    let external = feature("PY_STDLIB_EXTERNAL");
    writeln!(generated, "pub const EXTERNAL_DIR: &str = {:?};", lib.display().to_string()).unwrap();

    let mut files = Vec::new();
    if !external {
        collect(&lib, "", &mut files);
    }
    let mut rules = Vec::new();
    if !feature("PY_STDLIB_FULL") {
        parse_rules(&std::fs::read_to_string(manifest.join("stdlib-exclude.txt")).expect("read stdlib-exclude.txt"), &mut rules);
    }
    if let Ok(extra) = std::env::var("LUMEN_PY_STDLIB_EXCLUDE") {
        rules.extend(extra.split(',').map(str::trim).filter(|r| !r.is_empty()).map(str::to_string));
    }
    files.retain(|(rel, _)| !excluded(&rules, rel));

    let keep_comments = feature("PY_STDLIB_COMMENTS");
    let quality = if std::env::var("PROFILE").is_ok_and(|p| p != "debug") { 11 } else { 5 };

    let mut chunks: Vec<(u32, u32, u32)> = Vec::new();
    let mut index: Vec<(String, u32, u32, u32)> = Vec::new();
    let mut blob: Vec<u8> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut raw_total = 0usize;
    let mut unstripped = Vec::new();

    let flush = |cur: &mut Vec<u8>, blob: &mut Vec<u8>, chunks: &mut Vec<(u32, u32, u32)>| {
        if cur.is_empty() {
            return;
        }
        let packed = lumen_common::compress::brotli_compress(cur, quality, 22);
        chunks.push((blob.len() as u32, packed.len() as u32, cur.len() as u32));
        blob.extend_from_slice(&packed);
        cur.clear();
    };

    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        let src = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        raw_total += src.len();
        let body = if keep_comments {
            src
        } else {
            strip_comments(&src).unwrap_or_else(|| {
                unstripped.push(rel.clone());
                src
            })
        };
        if !cur.is_empty() && cur.len() + body.len() > CHUNK_TARGET {
            flush(&mut cur, &mut blob, &mut chunks);
        }
        index.push((rel.clone(), chunks.len() as u32, cur.len() as u32, body.len() as u32));
        cur.extend_from_slice(body.as_bytes());
    }
    flush(&mut cur, &mut blob, &mut chunks);

    if !unstripped.is_empty() {
        println!("cargo:warning=lumen-py: {} stdlib modules embedded with comments: {}", unstripped.len(), unstripped.join(", "));
    }
    println!("cargo:warning=lumen-py stdlib: {} files, {} source bytes, {} embedded bytes", files.len(), raw_total, blob.len());

    std::fs::write(out_dir.join("stdlib.br"), &blob).expect("write stdlib blob");
    writeln!(generated, "pub static BLOB: &[u8] = include_bytes!(concat!(env!(\"OUT_DIR\"), \"/stdlib.br\"));").unwrap();
    generated.push_str("/// `(offset, compressed length, uncompressed length)` of each chunk in `BLOB`.\n");
    generated.push_str("pub static CHUNKS: &[(u32, u32, u32)] = &[\n");
    for (o, c, u) in &chunks {
        writeln!(generated, "    ({o}, {c}, {u}),").unwrap();
    }
    generated.push_str("];\n/// `(path, chunk, offset in the chunk, length)` of each module.\n");
    generated.push_str("pub static FILES: &[(&str, u32, u32, u32)] = &[\n");
    for (rel, chunk, off, len) in &index {
        writeln!(generated, "    ({rel:?}, {chunk}, {off}, {len}),").unwrap();
    }
    generated.push_str("];\n");
    std::fs::write(out_dir.join("frozen_table.rs"), generated).expect("write frozen table");
}
