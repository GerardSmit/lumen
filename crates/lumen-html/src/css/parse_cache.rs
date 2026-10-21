//! Parsed stylesheets kept between rule rebuilds. A page that loads its
//! sheets one after another rebuilds the style index after every load; each
//! rebuild reuses the parse of every sheet whose text, import graph and live
//! topology are unchanged and parses only the new or edited one. Entries are
//! keyed by that content, so an edited sheet can never match its old entry.

use super::nesting::DeclarationBoundary;
use super::{
    parse_graph_topology, parse_stylesheet_with_boundaries, CssError, Declaration, FontFaceRule, MediaEnvironment,
    ParsedStylesheets, Rule, RuleTopologyOverride, StylesheetIdentity, StylesheetSource,
};
use alloc::{sync::Arc, vec::Vec};
use core::cell::RefCell;

const MAX_ENTRIES: usize = 48;
const MAX_BYTES: usize = 64 << 20;
const RULE_HEAP_ESTIMATE: usize = 256;

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
}

#[derive(Default)]
struct Inner {
    entries: Vec<Entry>,
    clock: u64,
    epoch: u64,
    parsed: u64,
    reused: u64,
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
    rules
        .saturating_add(keyframes)
        .saturating_add(parsed.font_faces.len().saturating_mul(core::mem::size_of::<FontFaceRule>()))
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
        {
            let mut inner = self.inner.borrow_mut();
            inner.clock += 1;
            let clock = inner.clock;
            let epoch = inner.epoch;
            if let Some(entry) = inner.entries.iter_mut().find(|entry| matches!(&entry.key,
                Key::Graph { source, topologies } if source == root && same_topologies(topologies, &relevant))) {
                entry.last_used = clock;
                entry.epoch = epoch;
                let parsed = entry.parsed.clone();
                inner.reused += 1;
                return Ok(parsed);
            }
        }
        let parsed = parse_graph_topology(root, environment, owner, topologies)?;
        self.insert(Key::Graph { source: root.clone(), topologies: relevant }, graph_bytes(root), &parsed);
        Ok(parsed)
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
        self.insert(Key::Text { text: Arc::from(text), boundaries: boundaries.to_vec() }, text.len(), &parsed);
        Ok(parsed)
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

    fn insert(&self, key: Key, bytes: usize, parsed: &ParsedStylesheets) {
        let mut inner = self.inner.borrow_mut();
        inner.parsed += 1;
        let bytes = bytes.saturating_add(parsed_bytes(parsed));
        if bytes > MAX_BYTES {
            return;
        }
        let last_used = inner.clock;
        let epoch = inner.epoch;
        inner.entries.push(Entry { key, parsed: parsed.clone(), bytes, last_used, epoch });
        let mut total: usize = inner.entries.iter().map(|entry| entry.bytes).fold(0, usize::saturating_add);
        while inner.entries.len() > 1 && (inner.entries.len() > MAX_ENTRIES || total > MAX_BYTES) {
            let Some(oldest) = inner.entries.iter().enumerate().min_by_key(|(_, entry)| entry.last_used).map(|(index, _)| index) else { break; };
            total = total.saturating_sub(inner.entries.swap_remove(oldest).bytes);
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
}
