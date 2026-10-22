//! Compiled `var()` substitution. A deferred declaration's raw text is scanned once into a
//! template of literal runs and variable references; substituting against a custom property map
//! then walks the template without tokenizing again. The parsed declarations of the substituted
//! text are cached by property and text, so every element whose map resolves to the same text
//! shares one parse. Every `var()` substitution in the engine goes through [`Template`], including
//! the per-cascade memoized expansion of custom properties.
use super::*;
use lumen_common::fasthash::{fnv1a64, FNV1A64_OFFSET};
use lumen_common::generations::Generations;

#[derive(Debug)]
enum Fallback {
    Absent,
    /// The fallback text cannot be substituted; using it fails the whole value.
    Invalid,
    Template(Box<Template>),
}

#[derive(Debug)]
enum Piece {
    /// Text copied verbatim. Quoted text and comment placeholders are not charged against the byte
    /// budget when copied, exactly as the textual scanner does.
    Literal { text: Box<str>, checked: bool },
    Var { name: Arc<str>, fallback: Fallback },
}

#[derive(Debug)]
pub(super) struct Template {
    pieces: Vec<Piece>,
}

fn push_literal(pieces: &mut Vec<Piece>, text: &str, checked: bool) {
    if text.is_empty() {
        return;
    }
    if let Some(Piece::Literal { text: last, checked: last_checked }) = pieces.last_mut() {
        if *last_checked == checked {
            let mut joined = String::with_capacity(last.len() + text.len());
            joined.push_str(last);
            joined.push_str(text);
            *last = joined.into_boxed_str();
            return;
        }
    }
    pieces.push(Piece::Literal { text: text.into(), checked });
}

impl Template {
    /// The template of `raw`; `None` when substituting it can never succeed.
    pub(super) fn compile(raw: &str) -> Option<Template> {
        if raw.len() > MAX_VARIABLE_BYTES {
            return None;
        }
        Self::scan(raw)
    }

    fn scan(raw: &str) -> Option<Template> {
        let bytes = raw.as_bytes();
        let mut pieces = Vec::new();
        let (mut pos, mut quote) = (0, 0u8);
        let special = |byte: &u8| matches!(byte, b'\'' | b'"' | b'/' | b'v' | b'V');
        while pos < bytes.len() {
            if quote == 0 && !special(&bytes[pos]) {
                let run = bytes[pos..].iter().position(special).unwrap_or(bytes.len() - pos);
                push_literal(&mut pieces, &raw[pos..pos + run], true);
                pos += run;
                continue;
            }
            if quote != 0 || matches!(bytes[pos], b'\'' | b'"') {
                let byte = bytes[pos];
                if quote == 0 {
                    quote = byte;
                } else if byte == quote {
                    quote = 0;
                }
                let start = pos;
                pos += raw[pos..].chars().next()?.len_utf8();
                if byte == b'\\' && pos < bytes.len() {
                    pos += raw[pos..].chars().next()?.len_utf8();
                }
                push_literal(&mut pieces, &raw[start..pos], false);
                continue;
            }
            if raw[pos..].starts_with("/*") {
                let end = raw[pos + 2..].find("*/")?;
                push_literal(&mut pieces, " ", false);
                pos += end + 4;
                continue;
            }
            let is_var = bytes.get(pos..pos + 3).is_some_and(|v| v.eq_ignore_ascii_case(b"var"))
                && bytes.get(pos + 3) == Some(&b'(')
                && (pos == 0 || !matches!(bytes[pos - 1], b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | 128..=255));
            if is_var {
                let (end, name, fallback) = variable_reference(raw, pos + 3)?;
                let fallback = match fallback {
                    None => Fallback::Absent,
                    Some(text) if text.len() > MAX_VARIABLE_BYTES => Fallback::Invalid,
                    Some(text) => Self::scan(text).map_or(Fallback::Invalid, |template| Fallback::Template(Box::new(template))),
                };
                pieces.push(Piece::Var { name: Arc::from(name), fallback });
                pos = end;
            } else {
                let ch = raw[pos..].chars().next()?;
                let mut buffer = [0u8; 4];
                push_literal(&mut pieces, ch.encode_utf8(&mut buffer), true);
                pos += ch.len_utf8();
            }
        }
        Some(Template { pieces })
    }

    /// The whole value is one `var()` reference, so its substitution is a custom property's own text.
    pub(super) fn is_single(&self) -> bool {
        matches!(self.pieces[..], [Piece::Var { .. }])
    }

    /// Whether every reference parses, with fallbacks nested at most `MAX_VARIABLE_DEPTH` deep.
    /// Unresolved names are valid.
    fn syntax_valid(&self, depth: usize) -> bool {
        depth < MAX_VARIABLE_DEPTH
            && self.pieces.iter().all(|piece| match piece {
                Piece::Literal { .. } => true,
                Piece::Var { fallback, .. } => match fallback {
                    Fallback::Absent => true,
                    Fallback::Invalid => false,
                    Fallback::Template(template) => template.syntax_valid(depth + 1),
                },
            })
    }

    /// The template substituted from `names`; `None` is the guaranteed-invalid value.
    pub(super) fn expand(&self, names: &dyn CustomNames) -> Option<Arc<str>> {
        if let [Piece::Var { name, .. }] = &self.pieces[..] {
            if let Some(value) = names.value(name).filter(|value| !value.expands()) {
                return (value.len() <= MAX_VARIABLE_BYTES).then(|| value.0.clone());
            }
        }
        let mut out = String::new();
        self.expand_into(names, &mut Vec::new(), &mut ExpandWalk::plain(), &mut out, MAX_VARIABLE_BYTES)?;
        Some(Arc::from(out))
    }

    fn expand_into(&self, names: &dyn CustomNames, stack: &mut Vec<Arc<str>>, walk: &mut ExpandWalk<'_>,
        out: &mut String, limit: usize) -> Option<()> {
        if stack.len() >= MAX_VARIABLE_DEPTH {
            walk.tainted = true;
            return None;
        }
        walk.deepest = walk.deepest.max(stack.len());
        for piece in &self.pieces {
            match piece {
                Piece::Literal { text, checked } => {
                    if *checked && out.len() + text.len() > limit {
                        return None;
                    }
                    out.push_str(text);
                }
                Piece::Var { name, fallback } => {
                    if !Self::resolve(name, names, stack, walk, out, limit)? {
                        let template = match fallback {
                            Fallback::Template(template) => template,
                            Fallback::Absent | Fallback::Invalid => return None,
                        };
                        let mut value = String::new();
                        template.expand_into(names, stack, walk, &mut value, MAX_VARIABLE_BYTES)?;
                        if out.len() + value.len() > limit {
                            return None;
                        }
                        out.push_str(&value);
                    }
                }
            }
        }
        Some(())
    }

    /// Appends the value of `name`. `Some(false)` when the name has no usable value, so its
    /// fallback applies; `None` when the byte budget is exceeded.
    fn resolve(name: &Arc<str>, names: &dyn CustomNames, stack: &mut Vec<Arc<str>>, walk: &mut ExpandWalk<'_>,
        out: &mut String, limit: usize) -> Option<bool> {
        if stack.iter().any(|entry| **entry == **name) {
            walk.tainted = true;
            return Some(false);
        }
        let Some(value) = names.value(name) else { return Some(false) };
        if !value.expands() {
            if out.len() + value.len() > limit {
                return None;
            }
            out.push_str(value);
            return Some(true);
        }
        let slot = if walk.memo.is_some() { names.slot(name) } else { None };
        stack.push(name.clone());
        let expanded = expand_framed(slot, value, names, stack, walk);
        stack.pop();
        let Some(expanded) = expanded else { return Some(false) };
        if out.len() + expanded.len() > limit {
            return None;
        }
        out.push_str(&expanded);
        Some(true)
    }
}

/// Expanded text per name for one cascade, so a token referenced by many others expands once.
/// An entry is kept only when its expansion neither hit the depth limit nor met a name already
/// on the stack, which makes it independent of how it was reached.
pub(super) struct ExpandMemo {
    done: Vec<Option<(Option<String>, usize)>>,
}

impl ExpandMemo {
    pub(super) fn new(slots: usize) -> Self { Self { done: alloc::vec![None; slots] } }
}

pub(super) struct ExpandWalk<'a> {
    memo: Option<&'a mut ExpandMemo>,
    deepest: usize,
    tainted: bool,
}

impl<'a> ExpandWalk<'a> {
    pub(super) fn plain() -> Self { Self { memo: None, deepest: 0, tainted: false } }

    pub(super) fn memoized(memo: &'a mut ExpandMemo) -> Self { Self { memo: Some(memo), deepest: 0, tainted: false } }
}

/// `raw` with its `var()` references substituted from `names`, within `limit` bytes of output;
/// `None` is the guaranteed-invalid value.
pub(super) fn expand_variables_in(raw: &str, names: &dyn CustomNames, limit: usize) -> Option<String> {
    expand_text(raw, names, &mut Vec::new(), &mut ExpandWalk::plain(), limit)
}

/// Whether the `var()` references of `raw` parse, ignoring whether their names resolve.
pub(super) fn references_parse(raw: &str, limit: usize) -> bool {
    raw.len() <= limit && Template::scan(raw).is_some_and(|template| template.syntax_valid(0))
}

fn expand_text(raw: &str, names: &dyn CustomNames, stack: &mut Vec<Arc<str>>, walk: &mut ExpandWalk<'_>,
    limit: usize) -> Option<String> {
    if stack.len() >= MAX_VARIABLE_DEPTH {
        walk.tainted = true;
        return None;
    }
    if raw.len() > limit {
        return None;
    }
    walk.deepest = walk.deepest.max(stack.len());
    if !needs_expansion(raw) {
        return Some(String::from(raw));
    }
    let template = Template::scan(raw)?;
    let mut out = String::with_capacity(raw.len());
    template.expand_into(names, stack, walk, &mut out, limit)?;
    Some(out)
}

/// Expands `value`, the text of the name that was just pushed on `stack` (or of a top-level
/// entry), through the memo slot of that name when it has one.
fn expand_framed(slot: Option<usize>, value: &str, names: &dyn CustomNames, stack: &mut Vec<Arc<str>>,
    walk: &mut ExpandWalk<'_>) -> Option<String> {
    let Some(slot) = slot.filter(|_| walk.memo.is_some()) else {
        return expand_text(value, names, stack, walk, MAX_VARIABLE_BYTES);
    };
    let known = walk.memo.as_ref().and_then(|memo| memo.done.get(slot)).and_then(Option::as_ref)
        .filter(|(_, extra)| stack.len() + extra < MAX_VARIABLE_DEPTH)
        .map(|(result, extra)| (result.clone(), *extra));
    if let Some((result, extra)) = known {
        walk.deepest = walk.deepest.max(stack.len() + extra);
        return result;
    }
    let entry = stack.len();
    let outer = (walk.deepest, walk.tainted);
    walk.deepest = entry;
    walk.tainted = false;
    let result = expand_text(value, names, stack, walk, MAX_VARIABLE_BYTES);
    if !walk.tainted {
        if let Some(memo) = walk.memo.as_mut() {
            memo.done[slot] = Some((result.clone(), walk.deepest - entry));
        }
    }
    walk.deepest = walk.deepest.max(outer.0);
    walk.tainted |= outer.1;
    result
}

/// Expands the top-level entry at `at` of `names`; the slot is used only when it is that entry's own.
pub(super) fn expand_entry(at: usize, name: &str, value: &str, names: &dyn CustomNames, stack: &mut Vec<Arc<str>>,
    walk: &mut ExpandWalk<'_>) -> Option<String> {
    let slot = names.slot(name).filter(|slot| *slot == at);
    stack.clear();
    expand_framed(slot, value, names, stack, walk)
}

#[derive(Clone)]
struct TemplateEntry {
    /// Keeps the source text alive so its address stays the key's identity.
    _raw: Arc<str>,
    template: Option<Arc<Template>>,
}

#[derive(Clone)]
struct PairEntry {
    _raw: Arc<str>,
    _custom: Option<Arc<CustomProperties>>,
    expanded: Option<Arc<str>>,
}

#[derive(Clone)]
struct ParsedEntry {
    property: Arc<str>,
    text: Arc<str>,
    sibling: Option<(usize, usize)>,
    /// The text names no sibling-index function, so one parse serves every position.
    sibling_free: bool,
    declarations: Option<Arc<[Declaration]>>,
}

pub(super) struct ParsedHit {
    pub(super) sibling_free: bool,
    pub(super) declarations: Option<Arc<[Declaration]>>,
}

/// What one source text substitutes to under one custom property map: the declarations each property parses it to. Only substitutions
/// free of sibling functions and `@function` calls are recorded.
pub(super) struct Substituted {
    _raw: Arc<str>,
    _custom: Option<Arc<CustomProperties>>,
    parsed: parallel::SyncRefCell<Vec<(Arc<str>, Arc<[Declaration]>)>>,
}

impl Substituted {
    pub(super) fn declarations(&self, property: &Arc<str>) -> Option<Arc<[Declaration]>> {
        self.parsed.borrow().iter().find(|(known, _)| Arc::ptr_eq(known, property)).map(|(_, declarations)| declarations.clone())
    }

    fn remember(&self, property: &Arc<str>, declarations: &Arc<[Declaration]>) {
        let mut parsed = self.parsed.borrow_mut();
        if !parsed.iter().any(|(known, _)| Arc::ptr_eq(known, property)) {
            parsed.push((property.clone(), declarations.clone()));
        }
    }
}

const TEMPLATES: usize = 4096;
const PAIRS: usize = 512;
const PARSES: usize = 2048;
const SUBSTITUTIONS: usize = 1024;

/// Everything `var()` substitution remembers: compiled templates by source text identity,
/// substituted text by (source, custom property map) identity for multi-reference values, and
/// parsed declarations by (property, substituted text).
#[derive(Default)]
pub(super) struct DeferredExpansions {
    templates: Generations<usize, TemplateEntry, TEMPLATES>,
    pairs: Generations<(usize, usize), PairEntry, PAIRS>,
    parsed: Generations<u64, Arc<[ParsedEntry]>, PARSES>,
    substitutions: Generations<(usize, usize), Arc<Substituted>, SUBSTITUTIONS>,
}

fn address<T: ?Sized>(value: &Arc<T>) -> usize {
    Arc::as_ptr(value) as *const u8 as usize
}

fn parse_key(property: &str, text: &str, sibling: Option<(usize, usize)>) -> u64 {
    let mut hash = fnv1a64(FNV1A64_OFFSET, property.as_bytes());
    hash = fnv1a64(hash, &[0xff]);
    hash = fnv1a64(hash, text.as_bytes());
    if let Some((position, count)) = sibling {
        hash = fnv1a64(hash, &[0xfe]);
        hash = fnv1a64(hash, &(position as u64).to_le_bytes());
        hash = fnv1a64(hash, &(count as u64).to_le_bytes());
    }
    hash
}

impl DeferredExpansions {
    /// `Some(None)` is a source text whose substitution can never succeed.
    pub(super) fn template(&mut self, raw: &Arc<str>) -> Option<Option<Arc<Template>>> {
        self.templates.get(&address(raw)).map(|entry| entry.template)
    }

    pub(super) fn insert_template(&mut self, raw: &Arc<str>, template: Option<Arc<Template>>) {
        self.templates.insert(address(raw), TemplateEntry { _raw: raw.clone(), template });
    }

    fn pair_key(raw: &Arc<str>, custom: &Option<Arc<CustomProperties>>) -> (usize, usize) {
        (address(raw), custom.as_ref().map_or(0, address))
    }

    pub(super) fn pair(&mut self, raw: &Arc<str>, custom: &Option<Arc<CustomProperties>>) -> Option<Option<Arc<str>>> {
        self.pairs.get(&Self::pair_key(raw, custom)).map(|entry| entry.expanded)
    }

    pub(super) fn insert_pair(&mut self, raw: &Arc<str>, custom: &Option<Arc<CustomProperties>>, expanded: Option<Arc<str>>) {
        self.pairs.insert(Self::pair_key(raw, custom),
            PairEntry { _raw: raw.clone(), _custom: custom.clone(), expanded });
    }

    pub(super) fn substituted(&mut self, raw: &Arc<str>, custom: &Option<Arc<CustomProperties>>) -> Option<Arc<Substituted>> {
        self.substitutions.get(&Self::pair_key(raw, custom))
    }

    pub(super) fn remember_substituted(&mut self, raw: &Arc<str>, custom: &Option<Arc<CustomProperties>>,
        property: Option<(&Arc<str>, &Arc<[Declaration]>)>) {
        let key = Self::pair_key(raw, custom);
        let entry = match self.substitutions.get(&key) {
            Some(entry) => entry,
            None => {
                let entry = Arc::new(Substituted { _raw: raw.clone(), _custom: custom.clone(),
                    parsed: Default::default() });
                self.substitutions.insert(key, entry.clone());
                entry
            }
        };
        if let Some((property, declarations)) = property {
            entry.remember(property, declarations);
        }
    }

    pub(super) fn parsed(&mut self, property: &str, text: &str, sibling: Option<(usize, usize)>) -> Option<ParsedHit> {
        let bucket = self.parsed.get(&parse_key(property, text, sibling))?;
        bucket.iter()
            .find(|entry| entry.sibling == sibling && *entry.property == *property && *entry.text == *text)
            .map(|entry| ParsedHit { sibling_free: entry.sibling_free, declarations: entry.declarations.clone() })
    }

    /// Remembers the parse of `property: text` for `sibling`. A position-specific parse also
    /// leaves a marker without declarations under the position-free key that records the text
    /// depends on the position.
    pub(super) fn insert_parsed(&mut self, property: &Arc<str>, text: &Arc<str>, sibling: Option<(usize, usize)>,
        sibling_free: bool, declarations: Arc<[Declaration]>) {
        self.upsert(ParsedEntry { property: property.clone(), text: text.clone(), sibling, sibling_free,
            declarations: Some(declarations) }, true);
        if sibling.is_some() {
            self.upsert(ParsedEntry { property: property.clone(), text: text.clone(), sibling: None,
                sibling_free, declarations: None }, false);
        }
    }

    fn upsert(&mut self, entry: ParsedEntry, replace: bool) {
        let key = parse_key(&entry.property, &entry.text, entry.sibling);
        let existing = self.parsed.get(&key);
        let mut entries: Vec<ParsedEntry> = existing.as_deref().map(<[ParsedEntry]>::to_vec).unwrap_or_default();
        let same = |other: &ParsedEntry| other.sibling == entry.sibling && other.property == entry.property && other.text == entry.text;
        if entries.iter().any(same) {
            if !replace {
                return;
            }
            entries.retain(|other| !same(other));
        }
        entries.push(entry);
        self.parsed.insert(key, entries.into());
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, Option<&str>)]) -> CustomProperties {
        CustomProperties::flat(entries.iter()
            .map(|(name, value)| (CustomText::from(*name), value.map(CustomText::from)))
            .collect())
    }

    fn expand(raw: &str, names: &CustomProperties) -> Option<String> {
        Template::compile(raw)?.expand(names).map(|text| String::from(&*text))
    }

    fn check(names: &CustomProperties, cases: &[(&str, Option<&str>)]) {
        for (raw, expected) in cases {
            assert_eq!(expand(raw, names).as_deref(), *expected, "{raw}");
            assert_eq!(expand_variables_in(raw, names, MAX_VARIABLE_BYTES).as_deref(), *expected, "{raw}");
        }
    }

    fn sample() -> CustomProperties {
        map(&[
            ("--a", Some("red")),
            ("--b", Some("var(--a) blue")),
            ("--c", Some("var(--d)")),
            ("--d", Some("var(--c)")),
            ("--e", Some("")),
            ("--n", None),
            ("--s", Some("'var(--a)'")),
            ("--u", Some("url(x.png)")),
            ("--deep", Some("var(--b, green)")),
        ])
    }

    #[test]
    fn plain_and_single_references() {
        check(&sample(), &[
            ("var(--a)", Some("red")),
            ("VAR(--a)", Some("red")),
            ("1px var(--a) 2px", Some("1px red 2px")),
            ("var(--a)var(--a)", Some("redred")),
            ("var( --a )", Some("red")),
            ("var(--missing)", None),
            ("var(--missing, 1px)", Some(" 1px")),
            ("var(--e)", Some("")),
            ("var(--e) x", Some(" x")),
            ("var(--n, fb)", Some(" fb")),
            ("no refs at all", Some("no refs at all")),
            ("", Some("")),
        ]);
    }

    #[test]
    fn nested_fallbacks_and_empty_fallback() {
        check(&sample(), &[
            ("var(--x, var(--y, var(--a)))", Some("  red")),
            ("var(--x, var(--y, 3px))", Some("  3px")),
            ("var(--x,)", Some("")),
            ("var(--x, )", Some(" ")),
            ("var(--x, var(--y))", None),
            ("var(--x, a, b)", Some(" a, b")),
            ("var(--a, var(--x))", Some("red")),
            ("var(--x, var(--y, ))", Some("  ")),
        ]);
    }

    #[test]
    fn cycles_and_chained_values() {
        check(&sample(), &[
            ("var(--c)", None),
            ("var(--c, safe)", Some(" safe")),
            ("var(--b)", Some("red blue")),
            ("var(--deep)", Some("red blue")),
            ("var(--d, var(--c, z))", Some("  z")),
            ("x var(--c) y", None),
        ]);
    }

    #[test]
    fn comments_strings_and_urls() {
        check(&sample(), &[
            ("a/* var(--a) */b", Some("a b")),
            ("'var(--a)' var(--a)", Some("'var(--a)' red")),
            ("\"it's\" var(--a)", Some("\"it's\" red")),
            ("'a\\'var(--a)'", Some("'a\\'var(--a)'")),
            ("url(var(--u))", Some("url(url(x.png))")),
            ("url(x) var(--u)", Some("url(x) url(x.png)")),
            ("/* unterminated var(--a)", None),
            ("'unterminated var(--a)", Some("'unterminated var(--a)")),
            ("var(--a /* c */)", Some("red")),
            ("var(--a /* c */, x)", Some("red")),
            ("var(--s)", Some("'var(--a)'")),
            ("xvar(--a)", Some("xvar(--a)")),
            ("-var(--a)", Some("-var(--a)")),
            ("var (--a)", Some("var (--a)")),
            ("var(--a", None),
            ("var(a)", None),
            ("var(--a x)", None),
            ("a / b var(--a)", Some("a / b red")),
            ("vzr(--a) var(--a)", Some("vzr(--a) red")),
        ]);
    }

    #[test]
    fn byte_budget() {
        let fill = |count: usize| "x".repeat(count);
        let names = map(&[("--big", Some(&fill(50))), ("--over", Some(&fill(51)))]);
        let head = alloc::format!("{} ", fill(MAX_VARIABLE_BYTES - 51));
        let exact = alloc::format!("{head}{}", fill(50));
        check(&names, &[
            (&alloc::format!("{head}var(--big)"), Some(&exact)),
            (&alloc::format!("{head}var(--over)"), None),
            (&alloc::format!("{head}var(--x, var(--big))"), None),
            (&alloc::format!("{head}var(--x,var(--big))"), Some(&exact)),
            (&alloc::format!("{head}var(--x,var(--over))"), None),
            (&alloc::format!("{head}{head}"), None),
        ]);
    }

    #[test]
    fn single_reference_shares_value_text() {
        let names = sample();
        let template = Template::compile("var(--a)").unwrap();
        assert!(template.is_single());
        assert_eq!(&*template.expand(&names).unwrap(), "red");
        assert!(!Template::compile("var(--a) x").unwrap().is_single());
    }

    #[test]
    fn syntax_check_ignores_unresolved_names() {
        for raw in ["var(--x)", "var(--x, var(--y, 1px))", "a/**/b", "plain"] {
            assert!(references_parse(raw, MAX_VARIABLE_BYTES), "{raw}");
        }
        for raw in ["var(--x", "var(a)", "var(--x, var(a))", "/* open"] {
            assert!(!references_parse(raw, MAX_VARIABLE_BYTES), "{raw}");
        }
        let nested = "var(--x,".repeat(MAX_VARIABLE_DEPTH + 1) + &")".repeat(MAX_VARIABLE_DEPTH + 1);
        assert!(!references_parse(&nested, MAX_VARIABLE_BYTES));
    }
}
