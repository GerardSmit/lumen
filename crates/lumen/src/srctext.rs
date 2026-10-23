//! The source text a parse records function text against.
//!
//! Ordinary code is one contiguous `Rc<str>`. A `new Function(params, body)` is the text
//! `<kind> anonymous(<params>\n) {\n<body>\n}`; copying a megabyte-scale body into that wrapper
//! costs a second copy that lives as long as any function parsed from it. [`Src::dynamic`]
//! instead keeps the body as the JS string it already is and the small head and tail beside it:
//! offsets are those of the virtual concatenation, so spans, positions and `toString` ranges are
//! the same as with a single string.

use crate::lstr::LStr;
use lumen_os::mmap::FileBytes;
use std::borrow::Cow;
use std::rc::{Rc, Weak};

const DYN_TAIL: &str = "\n}";
const DYN_BODY_MARK: &str = "\n) {\n";

/// UTF-8 text that lives in a file mapping (or a buffer read from a file), validated once. The
/// pages are clean and file-backed, so the OS can drop them under pressure instead of the
/// process holding them as private memory.
pub struct MappedText {
    text: &'static str,
    _owner: FileBytes,
}

impl MappedText {
    /// `None` when the bytes are not valid UTF-8.
    pub fn new(owner: FileBytes) -> Option<MappedText> {
        let text = std::str::from_utf8(owner.as_bytes()).ok()?;
        // SAFETY: the bytes live in the mapping or heap buffer `owner` holds, whose address does
        // not change when `owner` moves and which outlives `text`: the field is private and only
        // handed out borrowed from `&self`.
        let text: &'static str = unsafe { &*(text as *const str) };
        Some(MappedText { text, _owner: owner })
    }

    pub fn as_str(&self) -> &str {
        self.text
    }
}

pub struct DynSrc {
    head: Box<str>,
    body: LStr,
}

impl DynSrc {
    fn parts(&self) -> [&str; 3] {
        [&self.head, self.body.as_str(), DYN_TAIL]
    }
}

#[derive(Clone)]
pub enum Src {
    Flat(Rc<str>),
    Dyn(Rc<DynSrc>),
    Mapped(Rc<MappedText>),
}

pub enum SrcWeak {
    Flat(Weak<str>),
    Dyn(Weak<DynSrc>),
    Mapped(Weak<MappedText>),
}

impl SrcWeak {
    pub fn strong_count(&self) -> usize {
        match self {
            SrcWeak::Flat(w) => w.strong_count(),
            SrcWeak::Dyn(w) => w.strong_count(),
            SrcWeak::Mapped(w) => w.strong_count(),
        }
    }
}

/// The pieces of a source range, in order: one for a range inside a single segment.
pub struct Pieces<'a> {
    parts: [&'a str; 3],
    n: usize,
}

impl<'a> std::ops::Deref for Pieces<'a> {
    type Target = [&'a str];
    fn deref(&self) -> &[&'a str] {
        &self.parts[..self.n]
    }
}

impl From<Rc<str>> for Src {
    fn from(s: Rc<str>) -> Self {
        Src::Flat(s)
    }
}

impl PartialEq for Src {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Src::Flat(a), Src::Flat(b)) => a == b,
            (Src::Dyn(a), Src::Dyn(b)) => Rc::ptr_eq(a, b),
            (Src::Mapped(a), Src::Mapped(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Src {
    /// A source backed by a file mapping; `None` when it is not valid UTF-8.
    pub fn mapped(bytes: FileBytes) -> Option<Src> {
        MappedText::new(bytes).map(|m| Src::Mapped(Rc::new(m)))
    }

    /// The source of `<prefix> anonymous(<params>\n) {\n<body>\n}` with `body` shared, not copied.
    pub fn dynamic(prefix: &str, params: &str, body: LStr) -> Src {
        let mut head = String::with_capacity(prefix.len() + params.len() + 16);
        head.push_str(prefix);
        head.push_str(" anonymous(");
        head.push_str(params);
        head.push_str(DYN_BODY_MARK);
        Src::Dyn(Rc::new(DynSrc {
            head: head.into_boxed_str(),
            body,
        }))
    }

    pub fn len(&self) -> usize {
        match self {
            Src::Flat(s) => s.len(),
            Src::Dyn(d) => d.parts().iter().map(|p| p.len()).sum(),
            Src::Mapped(m) => m.text.len(),
        }
    }

    /// The contiguous text of a source that is one string (every typed / JSX parse).
    pub fn flat(&self) -> &Rc<str> {
        match self {
            Src::Flat(s) => s,
            Src::Dyn(_) => unreachable!("a dynamic function source is never typed or JSX"),
            Src::Mapped(_) => unreachable!("a mapped source is a classic script, never typed or JSX"),
        }
    }

    pub fn is_rc(&self, rc: &Rc<str>) -> bool {
        matches!(self, Src::Flat(s) if Rc::ptr_eq(s, rc))
    }

    pub fn is_char_boundary(&self, at: usize) -> bool {
        match self {
            Src::Flat(s) => s.is_char_boundary(at),
            Src::Mapped(m) => m.text.is_char_boundary(at),
            Src::Dyn(d) => {
                let mut lo = 0usize;
                for part in d.parts() {
                    let hi = lo + part.len();
                    if at <= hi {
                        return part.is_char_boundary(at - lo);
                    }
                    lo = hi;
                }
                false
            }
        }
    }

    /// Identity of the allocation shared by every function parsed from this source.
    pub fn key(&self) -> usize {
        match self {
            Src::Flat(s) => Rc::as_ptr(s) as *const u8 as usize,
            Src::Dyn(d) => Rc::as_ptr(d) as *const u8 as usize,
            Src::Mapped(m) => Rc::as_ptr(m) as *const u8 as usize,
        }
    }

    pub fn downgrade(&self) -> SrcWeak {
        match self {
            Src::Flat(s) => SrcWeak::Flat(Rc::downgrade(s)),
            Src::Dyn(d) => SrcWeak::Dyn(Rc::downgrade(d)),
            Src::Mapped(m) => SrcWeak::Mapped(Rc::downgrade(m)),
        }
    }

    /// Where the body of a synthesized `function anonymous(...) {\n` wrapper starts.
    pub fn body_start(&self) -> Option<u32> {
        match self {
            Src::Flat(s) => s.find(DYN_BODY_MARK).map(|at| (at + DYN_BODY_MARK.len()) as u32),
            Src::Dyn(d) => Some(d.head.len() as u32),
            Src::Mapped(_) => None,
        }
    }

    /// The text of `start..end` as the pieces it spans, `None` when out of bounds or not on
    /// character boundaries. Never empty: an empty range is one empty piece.
    pub fn pieces(&self, start: usize, end: usize) -> Option<Pieces<'_>> {
        if start > end || end > self.len() {
            return None;
        }
        let mut out = Pieces {
            parts: [""; 3],
            n: 0,
        };
        if start < end {
            let flat;
            let dynamic;
            let parts: &[&str] = match self {
                Src::Flat(s) => {
                    flat = [&**s];
                    &flat
                }
                Src::Dyn(d) => {
                    dynamic = d.parts();
                    &dynamic
                }
                Src::Mapped(m) => {
                    flat = [m.text];
                    &flat
                }
            };
            let mut lo = 0usize;
            for part in parts {
                let hi = lo + part.len();
                let (from, to) = (start.max(lo), end.min(hi));
                if from < to {
                    out.parts[out.n] = part.get(from - lo..to - lo)?;
                    out.n += 1;
                }
                lo = hi;
            }
        }
        out.n = out.n.max(1);
        Some(out)
    }

    /// `start..end` borrowed, `None` when it spans segments (or is out of bounds).
    pub fn get(&self, start: usize, end: usize) -> Option<&str> {
        let p = self.pieces(start, end)?;
        match p.n {
            1 => Some(p.parts[0]),
            _ => None,
        }
    }

    /// `start..end` borrowed when inside one segment, copied when it spans several.
    pub fn slice(&self, start: usize, end: usize) -> Option<Cow<'_, str>> {
        let p = self.pieces(start, end)?;
        Some(match p.n {
            1 => Cow::Borrowed(p.parts[0]),
            _ => Cow::Owned(p.concat()),
        })
    }

    /// The whole text (a copy for a dynamic source).
    pub fn text(&self) -> Cow<'_, str> {
        self.slice(0, self.len()).unwrap_or(Cow::Borrowed(""))
    }
}

/// A classic script's source as the embedder hands it over: heap text, or a file mapped
/// read-only (see [`ScriptSource::map_file`]).
pub struct ScriptSource(pub(crate) Src);

impl ScriptSource {
    /// Maps `path` read-only so the source costs no private memory. A file that is not valid
    /// UTF-8 is read and decoded lossily instead, as an owned source. The file must only ever be
    /// replaced by rename, never rewritten in place, while the source lives.
    pub fn map_file(path: &std::path::Path) -> std::io::Result<ScriptSource> {
        let bytes = match lumen_os::mmap::map_file_readonly(path)? {
            lumen_os::mmap::FileBytes::Owned(bytes) => bytes,
            mapped => {
                if let Some(src) = Src::mapped(mapped) {
                    return Ok(ScriptSource(src));
                }
                std::fs::read(path)?
            }
        };
        Ok(ScriptSource::from(Rc::<str>::from(String::from_utf8_lossy(&bytes).as_ref())))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.len() == 0
    }

    /// Whether the text lives in a file mapping rather than on the heap.
    pub fn is_mapped(&self) -> bool {
        matches!(self.0, Src::Mapped(_))
    }
}

impl From<Rc<str>> for ScriptSource {
    fn from(s: Rc<str>) -> Self {
        ScriptSource(Src::Flat(s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::stack_trace::LineTable;

    fn dynamic(prefix: &str, params: &str, body: &str) -> (Src, String) {
        let src = Src::dynamic(prefix, params, LStr::from(body));
        let flat = src.text().into_owned();
        (src, flat)
    }

    #[test]
    fn body_is_referenced_not_copied() {
        let body = LStr::from("return 1;");
        let at = body.as_str().as_ptr();
        let src = Src::dynamic("function", "a", body);
        let start = src.body_start().unwrap() as usize;
        assert_eq!(src.get(start, start + 9).unwrap().as_ptr(), at);
        assert_eq!(&*src.text(), "function anonymous(a\n) {\nreturn 1;\n}");
        assert!(src.get(0, src.len()).is_none());
        assert_eq!(src.slice(start - 2, start + 3).unwrap(), "{\nret");
        assert_eq!(src.slice(start + 7, src.len()).unwrap(), "1;\n}");
        assert_eq!(src.get(start, start).unwrap(), "");
        assert!(src.pieces(0, src.len() + 1).is_none());
    }

    #[test]
    fn line_table_matches_the_flat_text() {
        for body in ["a\nb", "a\r", "a\r\nb\r", "\u{2028}é\nx", ""] {
            let (src, flat) = dynamic("function", "a,\r\nb", body);
            let (seg, one) = (LineTable::build_src(&src), LineTable::build(&flat));
            for p in 0..=flat.len() as u32 {
                assert_eq!(seg.line_col(p), one.line_col(p), "{body:?} at {p}");
            }
        }
    }

    #[test]
    fn segmented_parse_matches_flat_parse() {
        let bodies = [
            "return a + b",
            "",
            "var x = ;",
            "}",
            "/*",
            "`abc",
            "\"abc",
            "a b",
            "return 1 +",
            "// comment",
            "<!-- html comment",
            "--> html comment",
            "#!hashbang\nreturn 1",
            "return a\r",
            "var s = '\u{2028}';\nreturn function inner() { return s }",
            "function* g() { yield 1 }",
            "'use strict'; with (a) {}",
            "let a = 1; var a;",
            "return /re/g.test(a) / 2",
            "return `t${a}` + a\n/x/g",
        ];
        for params in ["", "a, b", "a = function () {}, b = () => 1", "a, a"] {
            for body in bodies {
                let (src, flat) = dynamic("function", params, body);
                let seg = crate::parser::parse_dynamic_function_shared(src);
                let one = crate::parser::parse_dynamic_function(&flat);
                match (seg, one) {
                    (Ok(a), Ok(b)) => {
                        let outer = |p: Vec<crate::ast::Stmt>| match p.into_iter().next() {
                            Some(crate::ast::Stmt::FuncDecl(f)) => f,
                            _ => panic!("no function for {params:?} {body:?}"),
                        };
                        let (a, b) = (outer(a), outer(b));
                        assert_eq!(a.source.text(), b.source.text(), "{params:?} {body:?}");
                        assert_eq!(a.ensure_body().is_ok(), b.ensure_body().is_ok());
                    }
                    (Err(_), Err(_)) => {}
                    (a, b) => panic!(
                        "{params:?} {body:?}: segmented ok={} flat ok={}",
                        a.is_ok(),
                        b.is_ok()
                    ),
                }
            }
        }
    }
}
