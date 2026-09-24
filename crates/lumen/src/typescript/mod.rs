//! TypeScript: the engine parses `.ts` source itself (the parser's TypeScript mode, always
//! compiled), and — with the `typed` feature — analyzes its types for the typed tier.
//!
//! The parser's TypeScript mode has Node's strip-only semantics (`--experimental-strip-types`,
//! amaro / swc's `ts_strip`): erasable TypeScript is consumed and blanked in the source text in
//! place, so the executable AST is plain JavaScript and every byte offset (function text, lazily
//! parsed bodies, stack positions) is the file's; syntax that needs emitted JavaScript is
//! rejected with Node's message ([`error_code`] gives its `ERR_*` code). The types themselves
//! go to a per-source [`SideTable`] keyed by span, never into the AST, together with the spans of
//! the source's `/** … */` comments.
//!
//! - [`strip_types`]: Node's `module.stripTypeScriptTypes` (the blanked text).
//! - `parser::parse_module_ts` / `parser::parse_cjs_function(.., ts = true)` run TypeScript
//!   source directly (ES and CommonJS modules, both with file offsets).
//! - [`node_error_text`]: Node's code-frame `stack` for a rejected source.
//! - With `typed`: the analyzer ([`analyze`], [`TypeTable`], the checker behind `lumen typed`).

pub use crate::parser::{error_code, side_table_for, ParamTy, SideFn, SideTable, TsSpan};

pub mod options;
pub use options::CompilerOptions;

#[cfg(feature = "typed")]
mod analysis;
#[cfg(feature = "typed")]
pub mod ast;
#[cfg(feature = "typed")]
mod check;
#[cfg(feature = "typed")]
pub mod jsdoc;
#[cfg(feature = "typed")]
pub mod lexer;
#[cfg(feature = "typed")]
pub mod parser;
#[cfg(feature = "typed")]
pub mod table;
#[cfg(feature = "typed")]
pub mod types;
#[cfg(feature = "typed")]
mod walk;

#[cfg(feature = "typed")]
pub use analysis::*;
#[cfg(feature = "typed")]
pub use ast::Lang;
#[cfg(feature = "typed")]
pub use parser::{parse_jsdoc_type, parse_module, parse_type_text};
#[cfg(feature = "typed")]
pub use table::{
    tag, ClassLayout, FnTypes, Signature, Site, SiteFact, SoundnessNote, Subject, TKind, TypeTable,
};
#[cfg(feature = "typed")]
pub use types::{
    assignable_in, equivalent_in, is_assignable, FnType, ObjectType, Param, Property, Type,
};

/// The typed tier's whole-file type table for a parsed source (`FnSource::Range`'s `Rc<str>`):
/// the TypeScript file's analysis, or a JavaScript file's JSDoc analysis. Its offsets are file
/// byte offsets, which are the engine's function offsets for ES modules and for CommonJS
/// modules alike (both compile header-free), so `table.fn_at(range.start)` finds a function's
/// facts. Computed on first request and cached with the source's side table; `None` when the
/// source had no types or JSDoc, or did not analyze.
#[cfg(feature = "typed")]
pub fn type_table(src: &std::rc::Rc<str>) -> Option<std::rc::Rc<TypeTable>> {
    let any = crate::parser::side_analysis(src, |text, is_ts| {
        let opts = AnalyzeOptions {
            lang: if is_ts { Lang::Ts } else { Lang::Js },
            compiler: CompilerOptions::default(),
        };
        let a = analyze(text, &opts).ok()?;
        Some(std::rc::Rc::new(a.table) as std::rc::Rc<dyn std::any::Any>)
    })?;
    any.downcast::<TypeTable>().ok()
}

#[cfg(test)]
mod strip_tests;
#[cfg(all(test, feature = "typed"))]
mod tests;

/// `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`: TypeScript that needs a transform.
pub const UNSUPPORTED: &str = "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX";
/// `ERR_INVALID_TYPESCRIPT_SYNTAX`: source that does not parse.
pub const INVALID: &str = "ERR_INVALID_TYPESCRIPT_SYNTAX";

/// A TypeScript source [`strip_types`] rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripError {
    pub message: String,
    /// [`UNSUPPORTED`] for syntax that needs transformation, [`INVALID`] for a file that does
    /// not parse.
    pub code: Option<&'static str>,
    /// Byte offset of the offending construct (or token), and the construct's end (`offset`
    /// for a single token or a position).
    pub offset: u32,
    pub end: u32,
    /// 1-based line and column (in characters) of `offset`.
    pub line: u32,
    pub column: u32,
}

impl std::fmt::Display for StripError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(c) => write!(f, "SyntaxError [{c}]: {}", self.message),
            None => write!(f, "SyntaxError: {}", self.message),
        }
    }
}

impl std::error::Error for StripError {}

/// Byte offset of the start of 1-based `line`.
fn line_start(src: &str, line: u32) -> u32 {
    let mut cur = 1;
    if line <= 1 {
        return 0;
    }
    let mut it = src.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let brk = match c {
            '\r' => {
                if matches!(it.peek(), Some((_, '\n'))) {
                    it.next();
                }
                true
            }
            '\n' | '\u{2028}' | '\u{2029}' => true,
            _ => false,
        };
        if brk {
            cur += 1;
            if cur == line {
                return it.peek().map_or(src.len(), |p| p.0) as u32;
            }
        }
        let _ = i;
    }
    src.len() as u32
}

/// Node's `stripTypeScriptTypes` (strip mode): `code` with its TypeScript blanked, every byte
/// offset kept.
pub fn strip_types(code: &str) -> Result<String, StripError> {
    crate::parser::strip_types(code).map_err(|(e, span)| {
        let at = span.map(|s| s.0);
        let offset = at.unwrap_or_else(|| line_start(code, e.line));
        let end = span.map_or(offset, |s| s.1);
        let head = code.get(..offset as usize).unwrap_or(code);
        let line_begin = head
            .rfind(['\n', '\r', '\u{2028}', '\u{2029}'])
            .map_or(0, |i| {
                i + head[i..].chars().next().map_or(1, char::len_utf8)
            });
        let column = head[line_begin..].chars().count() as u32 + 1;
        let line = if at.is_some() {
            1 + head
                .char_indices()
                .filter(|&(i, c)| {
                    matches!(c, '\n' | '\u{2028}' | '\u{2029}')
                        || (c == '\r' && !head[i + 1..].starts_with('\n'))
                })
                .count() as u32
        } else {
            e.line.max(1)
        };
        StripError {
            code: Some(error_code(&e.message)),
            message: e.message,
            offset,
            end,
            line,
            column,
        }
    })
}

/// Node's text for a rejected TypeScript source, the `stack` of the SyntaxError it throws (V8's
/// `at` frames aside): `file:line`, a code frame marking `span` (the offending construct, or
/// `line`'s start when only the line is known), a blank line, then
/// `SyntaxError [ERR_…_TYPESCRIPT_SYNTAX]: message`.
pub fn node_error_text(
    file: &str,
    src: &str,
    span: Option<(u32, u32)>,
    line: u32,
    message: &str,
) -> String {
    let (start, end) = span.unwrap_or_else(|| {
        let s = line_start(src, line);
        (s, s)
    });
    let start = (start as usize).min(src.len());
    let end = (end as usize).clamp(start, src.len());
    // (byte start, text without the '\n')
    let mut lines: Vec<(usize, &str)> = Vec::new();
    let mut at = 0;
    for l in src.split('\n') {
        lines.push((at, l));
        at += l.len() + 1;
    }
    let line_of = |b: usize| lines.iter().rposition(|l| l.0 <= b).unwrap_or(0);
    let (sl, el) = (line_of(start), line_of(end));
    let show = |s: &str| s.trim_end_matches('\r').replace('\t', "    ");
    let mut frame: Vec<String> = Vec::new();
    // The line after the span, when there is one with text (kept as is, `\r` included).
    let next = lines
        .get(el + 1)
        .filter(|l| !l.1.is_empty())
        .map(|l| l.1.replace('\t', "    "));
    if sl == el {
        if sl > 0 {
            frame.push(show(lines[sl - 1].1));
        }
        let (ls, text) = lines[sl];
        frame.push(show(text));
        let body = text.trim_end_matches('\r');
        let col = show(&body[..(start - ls).min(body.len())]).chars().count();
        let mut width = show(&src[start..end]).chars().count();
        if width == 0 && start - ls < body.len() {
            width = 1;
        }
        if width > 0 {
            frame.push(format!("{}{}", " ".repeat(col), "^".repeat(width)));
        }
    } else {
        for (i, l) in lines
            .iter()
            .enumerate()
            .take(el + 1)
            .skip(sl.saturating_sub(1))
        {
            let mark = if i == sl || i == el { "  > " } else { "    " };
            frame.push(format!("{mark}{}", show(l.1)));
        }
    }
    if let Some(n) = next {
        frame.push(if sl == el { n } else { format!("    {n}") });
    }
    format!(
        "{file}:{}\n{}\n\nSyntaxError [{}]: {message}",
        sl + 1,
        frame.join("\n"),
        error_code(message)
    )
}

/// The report Node prints for an uncaught TypeScript SyntaxError whose `stack` is `stack` (see
/// [`node_error_text`]): the stack, then the error's `code` property.
pub fn node_uncaught_text(stack: &str, code: &str) -> String {
    format!("{stack} {{\n  code: '{code}'\n}}")
}

/// Whether `text` is a [`node_uncaught_text`] report (printed as is, with no `Uncaught` prefix).
pub fn is_node_uncaught_text(text: &str) -> bool {
    text.contains("_TYPESCRIPT_SYNTAX]: ") && text.ends_with("_TYPESCRIPT_SYNTAX'\n}")
}

/// Whether `path` names a TypeScript source (`.ts`, `.mts`, `.cts`; a query or fragment
/// ignored). `.d.ts` files count: they hold only erasable declarations.
pub fn is_ts_path(path: &str) -> bool {
    let p = path.split(['?', '#']).next().unwrap_or(path);
    p.ends_with(".ts") || p.ends_with(".mts") || p.ends_with(".cts")
}

/// The parameters of Node's CommonJS module wrapper.
pub const CJS_PARAMS: [&str; 5] = ["exports", "require", "module", "__filename", "__dirname"];

/// Whether TypeScript `src` parses as a CommonJS module body (Node's syntax detection runs a
/// `.ts` file outside any package `"type"` as ESM when it does not).
pub fn parses_as_commonjs(src: &str) -> bool {
    crate::parser::parse_cjs_function(src, &CJS_PARAMS, true).is_ok()
}
