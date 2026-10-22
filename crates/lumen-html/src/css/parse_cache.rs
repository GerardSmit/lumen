//! Parsed stylesheets kept between rule rebuilds. A page that loads its
//! sheets one after another rebuilds the style index after every load; each
//! rebuild reuses the parse of every sheet whose text, import graph and live
//! topology are unchanged and parses only the new or edited one. Entries are
//! keyed by that content, so an edited sheet can never match its old entry.

use super::nesting::DeclarationBoundary;
use super::{
    parse_graph_topology, parse_stylesheet_with_boundaries, CssError, Declaration, FontFaceRule, MediaEnvironment,
    ParsedStylesheets, Rule, RuleTopologyOverride, Selector, StylesheetIdentity, StylesheetSource,
};
use alloc::{sync::Arc, vec::Vec};
use core::cell::RefCell;

const MAX_ENTRIES: usize = 256;
const MAX_BYTES: usize = 32 << 20;
const RULE_HEAP_ESTIMATE: usize = core::mem::size_of::<Selector>() + 16 + 64;

enum Key {
    Graph {
        source: StylesheetSource,
        topologies: Vec<RuleTopologyOverride>,
    },
    Text {
        text: Arc<str>,
        boundaries: Vec<DeclarationBoundary>,
    },
}

struct Entry {
    key: Key,
    parsed: ParsedStylesheets,
    bytes: usize,
    last_used: u64,
    epoch: u64,
    /// Parsed elsewhere and not installed yet; eviction skips it so the
    /// install finds the parse instead of repeating it.
    pinned: bool,
}

struct Inner {
    entries: Vec<Entry>,
    clock: u64,
    epoch: u64,
    parsed: u64,
    reused: u64,
    max_bytes: usize,
}

impl Default for Inner {
    fn default() -> Self {
        Self { entries: Vec::new(), clock: 0, epoch: 0, parsed: 0, reused: 0, max_bytes: MAX_BYTES }
    }
}

/// Parse results of a session's stylesheets.
#[derive(Default)]
pub struct StylesheetParseCache {
    inner: RefCell<Inner>,
}

fn graph_bytes(source: &StylesheetSource) -> usize {
    source.imports.iter().filter_map(|loaded| loaded.source.as_deref()).fold(source.text.len(), |bytes, child| bytes.saturating_add(graph_bytes(child)))
}

/// What an entry keeps alive: the parsed rules dominate the text they came
/// from, and a sheet of many small rules costs far more than its length.
fn parsed_bytes(parsed: &ParsedStylesheets) -> usize {
    let rules = parsed.rules.iter().fold(0usize, |bytes, rule| {
        bytes
            .saturating_add(core::mem::size_of::<Rule>() + RULE_HEAP_ESTIMATE)
            .saturating_add(rule.declarations.len().saturating_mul(core::mem::size_of::<Declaration>()))
    });
    let keyframes = parsed.keyframes.iter().fold(0usize, |bytes, keyframes| {
        bytes.saturating_add(core::mem::size_of_val(keyframes)).saturating_add(keyframes.css_text.len().saturating_mul(2))
    });
    let index = parsed.rule_index.as_ref().map_or(0, |index| index.heap_bytes());
    rules
        .saturating_add(index)
        .saturating_add(keyframes)
        .saturating_add(parsed.font_faces.len().saturating_mul(core::mem::size_of::<FontFaceRule>()))
}

fn shell(parsed: &ParsedStylesheets) -> ParsedStylesheets {
    ParsedStylesheets {
        rules: Vec::new(),
        font_faces: parsed.font_faces.clone(),
        family_display: parsed.family_display.clone(),
        keyframes: parsed.keyframes.clone(),
        position_try: parsed.position_try.clone(),
        counter_styles: parsed.counter_styles.clone(),
        functions: parsed.functions.clone(),
        layers: parsed.layers.clone(),
        rule_index: None,
    }
}

fn same_topologies(left: &[RuleTopologyOverride], right: &[RuleTopologyOverride]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.owner == right.owner
                && left.source_url == right.source_url
                && left.source_text == right.source_text
                && left.boundaries == right.boundaries
        })
}

impl StylesheetParseCache {
    /// `(parses, reuses)` since the cache was created.
    pub fn counts(&self) -> (u64, u64) {
        let inner = self.inner.borrow();
        (inner.parsed, inner.reused)
    }

    /// [`parse_graph_with_topology`](super::parse_graph_with_topology) through the cache. The owner only
    /// selects which topology overrides apply, so a graph without overrides
    /// is shared between owners and between validation and installation.
    /// `environment` is not part of the key: graph parsing does not depend on
    /// it (media conditions are evaluated when the rules are indexed).
    pub fn parse_graph(
        &self,
        root: &StylesheetSource,
        environment: MediaEnvironment,
        owner: Option<StylesheetIdentity>,
        topologies: &[RuleTopologyOverride],
    ) -> Result<ParsedStylesheets, CssError> {
        let relevant: Vec<RuleTopologyOverride> = owner.map_or_else(Vec::new, |owner| topologies.iter().filter(|entry| entry.owner == owner).cloned().collect());
        if let Some(parsed) = self.lookup_graph(root, &relevant) {
            return Ok(parsed);
        }
        let parsed = parse_graph_topology(root, environment, owner, topologies)?;
        self.insert(Key::Graph { source: root.clone(), topologies: relevant }, graph_bytes(root), parsed.clone());
        Ok(parsed)
    }

    /// Parses `root` into the cache when it is not there yet, without handing
    /// the result out.
    pub fn ensure_graph(&self, root: &StylesheetSource, environment: MediaEnvironment) -> Result<(), CssError> {
        if self.touch_graph(root) {
            return Ok(());
        }
        let parsed = parse_graph_topology(root, environment, None, &[])?;
        self.insert(Key::Graph { source: root.clone(), topologies: Vec::new() }, graph_bytes(root), parsed);
        Ok(())
    }

    /// Stores the result of [`parse_graph`](super::parse_graph) for `root`,
    /// computed elsewhere (a worker thread), as if [`Self::parse_graph`] had
    /// parsed it without an owner.
    pub fn insert_graph(&self, root: &StylesheetSource, parsed: ParsedStylesheets) {
        if self.touch_graph(root) {
            return;
        }
        self.insert_with(Key::Graph { source: root.clone(), topologies: Vec::new() }, graph_bytes(root), parsed, true);
    }

    fn touch_graph(&self, root: &StylesheetSource) -> bool {
        self.find_graph(root, &[], |_| ()).is_some()
    }

    fn lookup_graph(&self, root: &StylesheetSource, relevant: &[RuleTopologyOverride]) -> Option<ParsedStylesheets> {
        self.find_graph(root, relevant, |entry| entry.parsed.clone())
    }

    fn find_graph<T>(&self, root: &StylesheetSource, relevant: &[RuleTopologyOverride], take: impl FnOnce(&Entry) -> T) -> Option<T> {
        let mut inner = self.inner.borrow_mut();
        inner.clock += 1;
        let clock = inner.clock;
        let epoch = inner.epoch;
        let entry = inner.entries.iter_mut().find(|entry| matches!(&entry.key,
            Key::Graph { source, topologies } if source == root && same_topologies(topologies, relevant)))?;
        entry.last_used = clock;
        entry.epoch = epoch;
        entry.pinned = false;
        let taken = take(entry);
        inner.reused += 1;
        Some(taken)
    }

    /// [`parse_stylesheet_with_boundaries`] through the cache.
    pub fn parse_text(&self, text: &str, boundaries: &[DeclarationBoundary]) -> Result<ParsedStylesheets, CssError> {
        {
            let mut inner = self.inner.borrow_mut();
            inner.clock += 1;
            let clock = inner.clock;
            let epoch = inner.epoch;
            if let Some(entry) = inner.entries.iter_mut().find(|entry| matches!(&entry.key,
                Key::Text { text: cached, boundaries: cached_boundaries } if **cached == *text && cached_boundaries.as_slice() == boundaries)) {
                entry.last_used = clock;
                entry.epoch = epoch;
                let parsed = entry.parsed.clone();
                inner.reused += 1;
                return Ok(parsed);
            }
        }
        let parsed = parse_stylesheet_with_boundaries(text, boundaries)?;
        self.insert(Key::Text { text: Arc::from(text), boundaries: boundaries.to_vec() }, text.len(), parsed.clone());
        Ok(parsed)
    }

    /// What [`Self::parse_graph`] would return for a graph without topology
    /// overrides, minus its rules, when the cache holds it.
    pub fn parse_graph_shell(&self, root: &StylesheetSource) -> Option<ParsedStylesheets> {
        self.find_graph(root, &[], |entry| shell(&entry.parsed))
    }

    /// What [`Self::parse_text`] would return without boundaries, minus its
    /// rules, when the cache holds it.
    pub fn parse_text_shell(&self, text: &str) -> Option<ParsedStylesheets> {
        let mut inner = self.inner.borrow_mut();
        inner.clock += 1;
        let (clock, epoch) = (inner.clock, inner.epoch);
        let entry = inner.entries.iter_mut().find(|entry| matches!(&entry.key,
            Key::Text { text: cached, boundaries } if **cached == *text && boundaries.is_empty()))?;
        entry.last_used = clock;
        entry.epoch = epoch;
        let parsed = shell(&entry.parsed);
        inner.reused += 1;
        Some(parsed)
    }

    /// Starts a rebuild of every installed sheet; [`Self::finish_rebuild`]
    /// then drops entries the rebuild did not use, so replaced sheets'
    /// source text is released instead of lingering until eviction.
    pub fn begin_rebuild(&self) {
        self.inner.borrow_mut().epoch += 1;
    }

    pub fn finish_rebuild(&self) {
        let mut inner = self.inner.borrow_mut();
        let epoch = inner.epoch;
        inner.entries.retain(|entry| entry.epoch == epoch);
    }

    fn insert(&self, key: Key, bytes: usize, parsed: ParsedStylesheets) {
        self.insert_with(key, bytes, parsed, false);
    }

    /// Evicts the least recently used entry, first among those the last
    /// rebuild did not use; pinned entries are never evicted, so the cache
    /// may exceed its budget by what is waiting to be installed.
    fn insert_with(&self, key: Key, bytes: usize, parsed: ParsedStylesheets, pinned: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.parsed += 1;
        let bytes = bytes.saturating_add(parsed_bytes(&parsed));
        if bytes > inner.max_bytes {
            return;
        }
        inner.clock += 1;
        let (last_used, epoch, max_bytes) = (inner.clock, inner.epoch, inner.max_bytes);
        inner.entries.push(Entry { key, parsed, bytes, last_used, epoch, pinned });
        let mut total: usize = inner.entries.iter().map(|entry| entry.bytes).fold(0, usize::saturating_add);
        while inner.entries.len() > 1 && (inner.entries.len() > MAX_ENTRIES || total > max_bytes) {
            let Some(victim) = inner.entries.iter().enumerate()
                .filter(|(_, entry)| !entry.pinned && entry.last_used != last_used)
                .min_by_key(|(_, entry)| (entry.epoch == epoch, entry.last_used))
                .map(|(index, _)| index) else { break; };
            total = total.saturating_sub(inner.entries.swap_remove(victim).bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(url: &str, text: &str) -> StylesheetSource {
        StylesheetSource { disabled: false, url: Arc::from(url), text: Arc::from(text), imports: Vec::new() }
    }

    #[test]
    fn unchanged_graphs_and_texts_are_parsed_once_and_edits_miss() {
        let cache = StylesheetParseCache::default();
        let environment = MediaEnvironment::default();
        let first = source("https://x.test/a.css", "a{width:1px}b{width:2px}");
        let parsed = cache.parse_graph(&first, environment, None, &[]).unwrap();
        assert_eq!(parsed.rules.len(), 2);
        let owner = Some(StylesheetIdentity::Dom(crate::html::parse("", 8).unwrap().root()));
        let again = cache.parse_graph(&first.clone(), environment, owner, &[]).unwrap();
        assert_eq!(again.rules.len(), 2);
        assert_eq!(cache.counts(), (1, 1), "the owner only selects topology overrides");
        let edited = source("https://x.test/a.css", "a{width:1px}");
        assert_eq!(cache.parse_graph(&edited, environment, None, &[]).unwrap().rules.len(), 1);
        assert_eq!(cache.counts(), (2, 1));
        let other_url = source("https://x.test/b.css", "a{width:1px}");
        cache.parse_graph(&other_url, environment, None, &[]).unwrap();
        assert_eq!(cache.counts(), (3, 1), "rules record their sheet's URL");
        cache.parse_text("a{width:1px}", &[]).unwrap();
        cache.parse_text("a{width:1px}", &[]).unwrap();
        assert_eq!(cache.counts(), (4, 2));
    }

    #[test]
    fn the_cache_is_bounded_and_evicts_the_least_recently_used_entry() {
        let cache = StylesheetParseCache::default();
        for index in 0..MAX_ENTRIES + 8 {
            cache.parse_text(&alloc::format!("a{{width:{index}px}}"), &[]).unwrap();
        }
        assert_eq!(cache.inner.borrow().entries.len(), MAX_ENTRIES);
        let parsed = cache.counts().0;
        cache.parse_text(&alloc::format!("a{{width:{}px}}", MAX_ENTRIES + 7), &[]).unwrap();
        assert_eq!(cache.counts().0, parsed, "the newest entry is kept");
        cache.parse_text("a{width:0px}", &[]).unwrap();
        assert_eq!(cache.counts().0, parsed + 1, "the oldest entry was evicted");
    }

    #[test]
    fn a_graph_parsed_elsewhere_serves_later_owner_lookups() {
        fn assert_send<T: Send>() {}
        assert_send::<ParsedStylesheets>();
        assert_send::<StylesheetSource>();
        let cache = StylesheetParseCache::default();
        let environment = MediaEnvironment::default();
        let sheet = source("https://x.test/a.css", "a{width:1px}b{width:2px}.c{color:red;margin:0 auto}");
        let outside = super::super::parse_graph(&sheet, environment).unwrap();
        cache.insert_graph(&sheet, outside);
        assert_eq!(cache.counts(), (1, 0));
        let owner = Some(StylesheetIdentity::Dom(crate::html::parse("", 8).unwrap().root()));
        let cached = cache.parse_graph(&sheet.clone(), environment, owner, &[]).unwrap();
        assert_eq!(cache.counts(), (1, 1));
        let direct = super::super::parse_graph(&sheet, environment).unwrap();
        assert_eq!(alloc::format!("{:?}", cached.rules), alloc::format!("{:?}", direct.rules));
        cache.ensure_graph(&sheet, environment).unwrap();
        cache.insert_graph(&sheet, direct);
        assert_eq!(cache.counts(), (1, 3));
        cache.ensure_graph(&source("https://x.test/b.css", "a{width:1px}"), environment).unwrap();
        assert_eq!(cache.counts(), (2, 3));
    }
    fn with_max_bytes(max_bytes: usize) -> StylesheetParseCache {
        let cache = StylesheetParseCache::default();
        cache.inner.borrow_mut().max_bytes = max_bytes;
        cache
    }

    fn entry_bytes(sheet: &StylesheetSource) -> usize {
        let probe = StylesheetParseCache::default();
        probe.parse_graph(sheet, MediaEnvironment::default(), None, &[]).unwrap();
        let bytes = probe.inner.borrow().entries[0].bytes;
        bytes
    }

    fn sheet_text(index: usize) -> alloc::string::String {
        (0..40).map(|rule| alloc::format!(".s{index}r{rule}{{width:{rule}px;height:{index}px}}")).collect()
    }

    #[test]
    fn preparsed_graphs_are_not_evicted_before_they_are_installed() {
        let environment = MediaEnvironment::default();
        let sheets: Vec<StylesheetSource> = (0..12).map(|index| source(&alloc::format!("https://x.test/{index}.css"), &sheet_text(index))).collect();
        let one = entry_bytes(&sheets[0]);
        let cache = with_max_bytes(one * 3);
        for sheet in &sheets {
            cache.insert_graph(sheet, super::super::parse_graph(sheet, environment).unwrap());
        }
        assert_eq!(cache.inner.borrow().entries.len(), sheets.len(), "pinned entries exceed the budget instead of being evicted");
        let parsed = cache.counts().0;
        for sheet in &sheets {
            cache.ensure_graph(sheet, environment).unwrap();
        }
        assert_eq!(cache.counts().0, parsed, "installing preparsed sheets parses nothing");
        assert!(cache.inner.borrow().entries.iter().all(|entry| !entry.pinned));
        cache.insert_graph(&source("https://x.test/extra.css", &sheet_text(99)), super::super::parse_graph(&source("https://x.test/extra.css", &sheet_text(99)), environment).unwrap());
        assert!(cache.inner.borrow().entries.len() <= 4, "installed entries are evictable again");
    }

    #[test]
    fn eviction_prefers_entries_the_last_rebuild_did_not_use() {
        let environment = MediaEnvironment::default();
        let (kept, stale, fresh) = (source("https://x.test/k.css", &sheet_text(1)), source("https://x.test/s.css", &sheet_text(2)), source("https://x.test/f.css", &sheet_text(3)));
        let one = entry_bytes(&kept);
        let cache = with_max_bytes(one * 2 + one / 2);
        cache.parse_graph(&kept, environment, None, &[]).unwrap();
        cache.parse_graph(&stale, environment, None, &[]).unwrap();
        cache.begin_rebuild();
        cache.parse_graph(&stale, environment, None, &[]).unwrap();
        cache.parse_graph(&kept, environment, None, &[]).unwrap();
        cache.begin_rebuild();
        cache.parse_graph(&kept, environment, None, &[]).unwrap();
        let parsed = cache.counts().0;
        cache.parse_graph(&fresh, environment, None, &[]).unwrap();
        cache.parse_graph(&kept, environment, None, &[]).unwrap();
        assert_eq!(cache.counts().0, parsed + 1, "the sheet the rebuild still uses survives");
    }
}
