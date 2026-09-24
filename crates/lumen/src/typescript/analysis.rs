//! The typed tier's analysis API (docs/typed-tier.md §3.1): [`analyze`] parses TypeScript or
//! JSDoc-annotated JavaScript with the analyzer's own front end ([`super::parser`]), runs the
//! soundness checker and builds the [`TypeTable`], keyed by the same byte offsets the engine's
//! `FnSource::Range` carries (the engine parses TypeScript in place, keeping every offset).

use std::path::Path;

use super::ast;
use super::ast::Lang;
use super::check;
use super::lexer;
use super::options::CompilerOptions;
use super::parser::{parse_module, parse_type_text};
use super::table::*;
use super::types::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    /// 1-based.
    pub line: u32,
    /// 1-based, in characters.
    pub column: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: u16,
    pub message: String,
    pub span: Span,
}

impl std::fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}: TS{}: {}",
            self.span.line, self.span.column, self.code, self.message
        )
    }
}

/// Maps byte offsets to 1-based line/column positions.
pub struct LineIndex<'s> {
    src: &'s str,
    starts: Vec<usize>,
}

impl<'s> LineIndex<'s> {
    pub fn new(src: &'s str) -> Self {
        let mut starts = vec![0];
        starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { src, starts }
    }

    /// `(line, column)`, both 1-based; the column counts characters.
    pub fn line_col(&self, offset: u32) -> (u32, u32) {
        let off = (offset as usize).min(self.src.len());
        let line = self.starts.partition_point(|&s| s <= off) - 1;
        let start = self.starts[line];
        let col = self
            .src
            .get(start..off)
            .map(|s| s.chars().count())
            .unwrap_or(off - start);
        (line as u32 + 1, col as u32 + 1)
    }

    pub fn span(&self, start: u32, end: u32) -> Span {
        let (line, column) = self.line_col(start);
        Span {
            start: start as usize,
            end: end as usize,
            line,
            column,
        }
    }

    pub fn diagnostic(
        &self,
        code: u16,
        message: impl Into<String>,
        start: u32,
        end: u32,
    ) -> Diagnostic {
        Diagnostic {
            code,
            message: message.into(),
            span: self.span(start, end),
        }
    }
}

/// Formats a number the way JavaScript's `String(n)` does for property keys.
pub fn fmt_num(n: f64) -> String {
    if n.is_nan() {
        "NaN".into()
    } else if n.is_infinite() {
        if n > 0.0 { "Infinity" } else { "-Infinity" }.into()
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// Parses a standalone TypeScript type expression.
pub fn parse_type_expression(source: &str) -> Result<Type, Diagnostic> {
    parse_type_text(source, false)
}

impl Lang {
    /// `.ts`/`.mts`/`.cts`/`.tsx` → `Ts`; `.js`/`.mjs`/`.cjs`/`.jsx` → `Js`.
    pub fn from_path(path: &Path) -> Option<Lang> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "ts" | "mts" | "cts" | "tsx" => Some(Lang::Ts),
            "js" | "mjs" | "cjs" | "jsx" => Some(Lang::Js),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalyzeOptions {
    pub lang: Lang,
    pub compiler: CompilerOptions,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            lang: Lang::Ts,
            compiler: CompilerOptions::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Analysis {
    pub table: TypeTable,
    pub lang: Lang,
    pub compiler: CompilerOptions,
}

/// Parses and checks one file. The result's offsets are byte offsets into `src`, which are
/// also the offsets in `strip_types(src)` and in the text the engine compiles for an ES
/// module or a CommonJS module (compiled header-free, so function offsets are file offsets).
pub fn analyze(src: &str, opts: &AnalyzeOptions) -> Result<Analysis, Diagnostic> {
    let module = parse_module(src, opts.lang)?;
    let table = analyze_module(&module, src, &opts.compiler);
    Ok(Analysis {
        table,
        lang: opts.lang,
        compiler: opts.compiler.clone(),
    })
}

/// Builds the type table for an already parsed module.
pub fn analyze_module(module: &ast::Module, src: &str, compiler: &CompilerOptions) -> TypeTable {
    let mut program = check::Program::new(module, src, compiler);
    check::build_table(&mut program)
}

fn fn_label(f: &FnTypes) -> String {
    let params: Vec<String> = f
        .param_names
        .iter()
        .zip(&f.params)
        .map(|(n, k)| format!("{n}: {k}"))
        .collect();
    format!("{}({}): {}", f.name, params.join(", "), f.ret)
}

/// The `--typed-report` text (docs/typed-tier.md §4.7): one line per class and function,
/// in source order, with the first reason for a "not sound" verdict.
pub fn report_text(table: &TypeTable, src: &str, file: &str) -> String {
    let li = LineIndex::new(src);
    let pos = |off: u32| {
        let (l, c) = li.line_col(off);
        format!("{file}:{l}:{c}")
    };
    let mut rows: Vec<(u32, String, String)> = Vec::new();
    for (i, c) in table.classes.iter().enumerate() {
        let fields: Vec<String> = c
            .full_fields(&table.classes)
            .iter()
            .map(|(n, k, ro)| format!("{}{n}: {k}", if *ro { "readonly " } else { "" }))
            .collect();
        let verdict = match table.note_for(Subject::Class(i as u32)) {
            Some(n) if !c.sound => format!("layout not sound: {} ({})", n.reason, pos(n.at)),
            _ if !c.sound => "layout not sound: parent layout is not sound".to_string(),
            _ => format!("layout: {} fields ({})", fields.len(), fields.join(", ")),
        };
        rows.push((c.start, format!("class {}", c.name), verdict));
    }
    for (key, f) in &table.fns {
        let verdict = if f.sound {
            "sound".to_string()
        } else {
            match table.note_for(Subject::Fn(*key)) {
                Some(n) => format!("not sound: {} ({})", n.reason, pos(n.at)),
                None => "not sound".to_string(),
            }
        };
        rows.push((*key, fn_label(f), verdict));
    }
    rows.sort_by_key(|r| r.0);
    let w_pos = rows.iter().map(|r| pos(r.0).len()).max().unwrap_or(0);
    let w_name = rows.iter().map(|r| r.1.len()).max().unwrap_or(0).min(48);
    let mut out = String::new();
    for (off, name, verdict) in &rows {
        out.push_str(&format!(
            "{:<w_pos$}  {:<w_name$}  {verdict}\n",
            pos(*off),
            name
        ));
    }
    let (sound, total) = table.sound_count();
    out.push_str(&format!("{file}: {sound}/{total} functions sound\n"));
    out
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn kind_json(k: &TKind) -> String {
    json_str(&k.to_string())
}

fn site_json(s: &Site) -> String {
    let fact = match &s.fact {
        SiteFact::Field { class, index } => {
            format!("\"kind\":\"field\",\"class\":{class},\"index\":{index}")
        }
        SiteFact::Elem(k) => format!("\"kind\":\"elem\",\"type\":{}", kind_json(k)),
        SiteFact::Callee(c) => format!("\"kind\":\"callee\",\"fn\":{c}"),
        SiteFact::Check(k) => format!("\"kind\":\"check\",\"type\":{}", kind_json(k)),
    };
    format!("{{\"start\":{},\"end\":{},{fact}}}", s.start, s.end)
}

/// The `--typed-report=json` output: the whole table (facts included) plus reasons with
/// line/column positions.
pub fn report_json(table: &TypeTable, src: &str, file: &str) -> String {
    let li = LineIndex::new(src);
    let loc = |off: u32| {
        let (l, c) = li.line_col(off);
        format!("\"line\":{l},\"column\":{c}")
    };
    let mut fns = Vec::new();
    for (key, f) in &table.fns {
        let note = table
            .note_for(Subject::Fn(*key))
            .filter(|_| !f.sound)
            .map(|n| {
                format!(
                    ",\"reason\":{},\"reasonAt\":{{\"offset\":{},{}}}",
                    json_str(&n.reason),
                    n.at,
                    loc(n.at)
                )
            })
            .unwrap_or_default();
        let params: Vec<String> = f
            .param_names
            .iter()
            .zip(&f.params)
            .map(|(n, k)| format!("{{\"name\":{},\"type\":{}}}", json_str(n), kind_json(k)))
            .collect();
        let locals: Vec<String> = f
            .locals
            .iter()
            .map(|(o, k)| format!("{{\"offset\":{o},\"type\":{}}}", kind_json(k)))
            .collect();
        let sites: Vec<String> = f.sites.iter().map(site_json).collect();
        fns.push(format!(
            "{{\"name\":{},\"kind\":{},\"start\":{},\"end\":{},{},\"sound\":{}{note},\"params\":[{}],\"this\":{},\"ret\":{},\"sig\":{},\"locals\":[{}],\"sites\":[{}]}}",
            json_str(&f.name),
            json_str(&format!("{:?}", f.kind).to_ascii_lowercase()),
            f.start,
            f.end,
            loc(f.start),
            f.sound,
            params.join(","),
            kind_json(&f.this),
            kind_json(&f.ret),
            f.sig,
            locals.join(","),
            sites.join(",")
        ));
    }
    let mut classes = Vec::new();
    for (i, c) in table.classes.iter().enumerate() {
        let note = table
            .note_for(Subject::Class(i as u32))
            .filter(|_| !c.sound)
            .map(|n| {
                format!(
                    ",\"reason\":{},\"reasonAt\":{{\"offset\":{},{}}}",
                    json_str(&n.reason),
                    n.at,
                    loc(n.at)
                )
            })
            .unwrap_or_default();
        let fields: Vec<String> = c
            .fields
            .iter()
            .map(|(n, k, ro)| {
                format!(
                    "{{\"name\":{},\"type\":{},\"readonly\":{ro}}}",
                    json_str(n),
                    kind_json(k)
                )
            })
            .collect();
        let methods: Vec<String> = c
            .methods
            .iter()
            .map(|(n, o)| format!("{{\"name\":{},\"fn\":{o}}}", json_str(n)))
            .collect();
        classes.push(format!(
            "{{\"id\":{i},\"name\":{},\"start\":{},\"end\":{},{},\"parent\":{},\"sound\":{}{note},\"fields\":[{}],\"methods\":[{}]}}",
            json_str(&c.name),
            c.start,
            c.end,
            loc(c.start),
            c.parent.map(|p| p.to_string()).unwrap_or_else(|| "null".into()),
            c.sound,
            fields.join(","),
            methods.join(",")
        ));
    }
    let sigs: Vec<String> = table
        .sigs
        .iter()
        .map(|s| {
            let ps: Vec<String> = s.params.iter().map(kind_json).collect();
            format!(
                "{{\"params\":[{}],\"ret\":{}}}",
                ps.join(","),
                kind_json(&s.ret)
            )
        })
        .collect();
    let (sound, total) = table.sound_count();
    format!(
        "{{\"file\":{},\"sound\":{sound},\"total\":{total},\"functions\":[{}],\"classes\":[{}],\"signatures\":[{}]}}\n",
        json_str(file),
        fns.join(","),
        classes.join(","),
        sigs.join(",")
    )
}

/// A human-readable dump of every fact (params, locals and sites with their source text).
pub fn dump_facts(table: &TypeTable, src: &str) -> String {
    let li = LineIndex::new(src);
    let text = |s: u32, e: u32| -> String {
        let t = src.get(s as usize..e as usize).unwrap_or("?");
        let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if t.chars().count() > 40 {
            let cut: String = t.chars().take(37).collect();
            format!("{cut}...")
        } else {
            t
        }
    };
    let mut out = String::new();
    for (i, c) in table.classes.iter().enumerate() {
        let (l, col) = li.line_col(c.start);
        out.push_str(&format!(
            "class #{i} {} @{} ({l}:{col}) sound={} parent={:?}\n",
            c.name, c.start, c.sound, c.parent
        ));
        for (n, k, ro) in &c.fields {
            out.push_str(&format!(
                "  field {}{n}: {k}\n",
                if *ro { "readonly " } else { "" }
            ));
        }
        for (n, o) in &c.methods {
            out.push_str(&format!("  method {n} -> fn@{o}\n"));
        }
    }
    for (key, f) in &table.fns {
        let (l, col) = li.line_col(*key);
        out.push_str(&format!(
            "fn @{key}..{} ({l}:{col}) {} sound={} this={} sig=#{}\n",
            f.end,
            fn_label(f),
            f.sound,
            f.this,
            f.sig
        ));
        for (o, k) in &f.locals {
            out.push_str(&format!(
                "  local @{o} `{}`: {k}\n",
                text(*o, *o + ident_len(src, *o))
            ));
        }
        for s in &f.sites {
            let what = match &s.fact {
                SiteFact::Field { class, index } => format!("field class#{class}[{index}]"),
                SiteFact::Elem(k) => format!("elem {k}"),
                SiteFact::Callee(c) => format!("callee fn@{c}"),
                SiteFact::Check(k) => format!("check {k}"),
            };
            out.push_str(&format!(
                "  site @{}..{} `{}`: {what}\n",
                s.start,
                s.end,
                text(s.start, s.end)
            ));
        }
    }
    for (i, s) in table.sigs.iter().enumerate() {
        let ps: Vec<String> = s.params.iter().map(|k| k.to_string()).collect();
        out.push_str(&format!("sig #{i} ({}) => {}\n", ps.join(", "), s.ret));
    }
    out
}

fn ident_len(src: &str, off: u32) -> u32 {
    src.get(off as usize..)
        .map(|s| {
            s.char_indices()
                .find(|(_, c)| !lexer::is_id_part(*c))
                .map(|(i, _)| i)
                .unwrap_or(s.len())
        })
        .unwrap_or(0) as u32
}
