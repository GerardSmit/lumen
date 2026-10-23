//! Style sharing. Two elements share a computed style without selector
//! matching or the cascade when their parents have equal styles and their
//! `ShareClass`es are equal: the same tag, namespace and attributes any
//! selector or UA hint reads, under ancestors that are equal in the same
//! sense. Rules that read more than that (structure, siblings, `:has()`,
//! interaction and form state, the top layer) are evaluated on every
//! candidate element and must match identically.

use super::*;
use alloc::collections::{BTreeMap, BTreeSet};
use core::hash::{Hash, Hasher};

const MAX_REVALIDATED: usize = 4096;
const MAX_CHAIN: u32 = 256;
pub(super) const SHARED_STYLES: usize = 256;

struct Revalidated {
    order: usize,
    tag: Option<String>,
    class: Option<String>,
    id: Option<String>,
}

impl Revalidated {
    fn of(order: usize, selector: &Selector) -> Self {
        let keyed = !(selector.host || selector.slotted || selector.part.is_some());
        Self {
            order,
            tag: selector.tag.as_ref()
                .filter(|tag| keyed && *tag != "*" && !tag.contains(['|', ':']))
                .cloned(),
            class: selector.classes.first().filter(|_| keyed).cloned(),
            id: selector.id.as_ref().filter(|_| keyed).cloned(),
        }
    }
}

/// What the rules of an index let elements share.
pub(super) struct ShareKeys {
    rules: usize,
    usable: bool,
    tested: BTreeSet<String>,
    revalidated: Vec<Revalidated>,
}

impl ShareKeys {
    fn of(index: &StyleIndex) -> Self {
        let mut keys = Self { rules: index.rules.len(), usable: false, tested: BTreeSet::new(), revalidated: Vec::new() };
        if index.rules.iter().any(|rule| rule.scope.is_some() || rule.css_scope.is_some()) || index.has_container_rules() { return keys; }
        for (order, rule) in index.rules.iter().enumerate() {
            let selector = &*rule.selector;
            if rule.registration.is_some() || rule.starting_style || selector.pseudo_element.is_some()
                || !selector.state_dependent(true) {
                continue;
            }
            if keys.revalidated.len() >= MAX_REVALIDATED { return keys; }
            keys.revalidated.push(Revalidated::of(order, selector));
        }
        keys.tested = index.with_invalidation(|sets| sets.tested_attributes());
        keys.usable = true;
        keys
    }

    fn reads(&self, key: &str, image: bool) -> bool {
        if key.eq_ignore_ascii_case("class") { return true; }
        let inert = inert_attribute(key) || matches!(key, "title" | "for" | "name")
            || (image && matches!(key, "src" | "srcset" | "alt" | "loading" | "decoding" | "fetchpriority"));
        if !inert { return true; }
        if key.bytes().any(|byte| byte.is_ascii_uppercase()) {
            self.tested.contains(&key.to_ascii_lowercase())
        } else {
            self.tested.contains(key)
        }
    }

    fn relevant<'a>(&'a self, image: bool, attributes: &'a [(Name, String)]) -> impl Iterator<Item = (&'a str, &'a str)> + 'a {
        attributes.iter()
            .filter(move |(key, _)| self.reads(key.as_str(), image))
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }

    /// The orders of the rules that read more than the element and its
    /// ancestors' tested attributes and match `node` now.
    pub(super) fn revalidate(&self, index: &StyleIndex, document: &Document, node: NodeId, kind: &NodeKind) -> Vec<u32> {
        let mut matched = Vec::new();
        let NodeKind::Element { name, attributes, .. } = kind else { return matched };
        if self.revalidated.is_empty() { return matched; }
        let local = crate::svg::local_name(name);
        let attribute = |wanted: &str| attributes.iter()
            .find(|(key, _)| key.as_str().eq_ignore_ascii_case(wanted)).map_or("", |(_, value)| value.as_str());
        let (classes, id) = (attribute("class"), attribute("id"));
        let tree = SelectorDocument::ordinary(document);
        for rule in &self.revalidated {
            if rule.tag.as_deref().is_some_and(|tag| !tag.eq_ignore_ascii_case(local))
                || rule.class.as_deref().is_some_and(|class| !classes.split_ascii_whitespace().any(|other| other.eq_ignore_ascii_case(class)))
                || rule.id.as_deref().is_some_and(|wanted| !wanted.eq_ignore_ascii_case(id)) {
                continue;
            }
            if index.rules[rule.order].match_scope_proximity_in_tree(&tree, node, &crate::forms::NoValidityOverrides).is_some() {
                matched.push(rule.order as u32);
            }
        }
        matched
    }
}

impl StyleIndex {
    /// The sharing keys of the current rules, `None` when elements cannot
    /// share styles in this index.
    pub(super) fn style_sharing(&self) -> Option<Arc<ShareKeys>> {
        if !self.effect_declarations.is_empty() || !self.snapshot_declarations.is_empty() { return None; }
        let rules = self.rules.len();
        if let Some(keys) = self.share_keys.borrow().as_ref().filter(|keys| keys.rules == rules) {
            return keys.usable.then(|| keys.clone());
        }
        let keys = Arc::new(ShareKeys::of(self));
        *self.share_keys.borrow_mut() = Some(keys.clone());
        keys.usable.then_some(keys)
    }
}

enum Eligibility { Full, SelfOnly, Never }

fn eligibility(document: &Document, node: NodeId, namespace: &Namespace, name: &Name, attributes: &[(Name, String)], parent: NodeId) -> Eligibility {
    let tag = crate::svg::local_name(name);
    if tag == "slot" { return Eligibility::Never; }
    if document.interaction_state().focus_visible == Some(node) { return Eligibility::Never; }
    if namespace == &Namespace::Html {
        if matches!(tag, "img" | "source") && html_rendering_tag(document, parent) == Some("picture") { return Eligibility::Never; }
        if tag == "input" && crate::forms::input_type_state(document, node) == "tel" { return Eligibility::Never; }
    }
    let mut own = false;
    let mut inline = false;
    for (key, _) in attributes {
        let key = key.as_str();
        inline |= key == "style";
        own |= key == "popover";
    }
    if inline && document.inline_cssom_style(node).is_some() { own = true; }
    if own || inline { Eligibility::SelfOnly } else { Eligibility::Full }
}

struct ShareSig {
    parent: u32,
    unique: Option<NodeId>,
    namespace: Namespace,
    name: String,
    attributes: Vec<(String, String)>,
    shareable: bool,
}

/// Per node, the class of elements it shares styles with. Valid for one
/// document state: the owner clears it whenever a node's cascade inputs may
/// have changed.
#[derive(Default)]
pub(super) struct ShareClasses {
    nodes: Vec<(u32, u32)>,
    sigs: Vec<ShareSig>,
    lookup: BTreeMap<u64, Vec<u32>>,
}

impl ShareClasses {
    pub(super) fn clear(&mut self) {
        if self.sigs.is_empty() { return; }
        *self = Self::default();
    }

    pub(super) fn shareable(&self, class: u32) -> bool {
        class != 0 && self.sigs[class as usize - 1].shareable
    }

    pub(super) fn class_of(&mut self, document: &Document, keys: &ShareKeys, node: NodeId) -> u32 {
        self.resolve(document, keys, node, 0)
    }

    fn resolve(&mut self, document: &Document, keys: &ShareKeys, node: NodeId, depth: u32) -> u32 {
        if let Some(&(generation, class)) = self.nodes.get(node.index()) {
            if class != 0 && generation == node.generation { return class; }
        }
        let class = self.compute(document, keys, node, depth);
        if self.nodes.len() <= node.index() {
            self.nodes.resize(document.node_count().max(node.index() + 1), (0, 0));
        }
        self.nodes[node.index()] = (node.generation, class);
        class
    }

    fn compute(&mut self, document: &Document, keys: &ShareKeys, node: NodeId, depth: u32) -> u32 {
        let Ok(NodeKind::Element { namespace, name, attributes }) = document.kind(node) else { return self.unique(node) };
        let Some(parent) = document.parent(node).ok().flatten() else { return self.unique(node) };
        let parent_class = if depth < MAX_CHAIN && matches!(document.kind(parent), Ok(NodeKind::Element { .. })) {
            self.resolve(document, keys, parent, depth + 1)
        } else {
            self.unique(parent)
        };
        match eligibility(document, node, namespace, name, attributes, parent) {
            Eligibility::Never => self.unique(node),
            Eligibility::Full => self.element(keys, parent_class, namespace, name, attributes, true),
            Eligibility::SelfOnly => self.element(keys, parent_class, namespace, name, attributes, false),
        }
    }

    fn find_or_push(&mut self, hash: u64, same: impl Fn(&ShareSig) -> bool, make: impl FnOnce() -> ShareSig) -> u32 {
        if let Some(candidates) = self.lookup.get(&hash) {
            if let Some(&class) = candidates.iter().find(|&&class| same(&self.sigs[class as usize - 1])) { return class; }
        }
        self.sigs.push(make());
        let class = self.sigs.len() as u32;
        self.lookup.entry(hash).or_default().push(class);
        class
    }

    fn unique(&mut self, node: NodeId) -> u32 {
        let mut hasher = MatchedHasher(0);
        node.hash(&mut hasher);
        self.find_or_push(hasher.finish(), |sig| sig.unique == Some(node), || ShareSig {
            parent: 0,
            unique: Some(node),
            namespace: Namespace::Html,
            name: String::new(),
            attributes: Vec::new(),
            shareable: false,
        })
    }

    fn element(&mut self, keys: &ShareKeys, parent: u32, namespace: &Namespace, name: &Name, attributes: &[(Name, String)], shareable: bool) -> u32 {
        let tag = name.as_str();
        let image = namespace == &Namespace::Html && crate::svg::local_name(name) == "img";
        let mut hasher = MatchedHasher(0);
        parent.hash(&mut hasher);
        match namespace {
            Namespace::Html => 0u8.hash(&mut hasher),
            Namespace::Svg => 1u8.hash(&mut hasher),
            Namespace::MathMl => 2u8.hash(&mut hasher),
            Namespace::Other(uri) => uri.hash(&mut hasher),
        }
        tag.hash(&mut hasher);
        shareable.hash(&mut hasher);
        for (key, value) in keys.relevant(image, attributes) {
            key.hash(&mut hasher);
            value.hash(&mut hasher);
        }
        let same = |sig: &ShareSig| {
            if sig.unique.is_some() || sig.parent != parent || sig.shareable != shareable
                || sig.namespace != *namespace || sig.name != tag {
                return false;
            }
            let mut stored = sig.attributes.iter();
            keys.relevant(image, attributes)
                .all(|(key, value)| stored.next().is_some_and(|(other_key, other_value)| other_key == key && other_value == value))
                && stored.next().is_none()
        };
        let make = || ShareSig {
            parent,
            unique: None,
            namespace: namespace.clone(),
            name: tag.to_string(),
            attributes: keys.relevant(image, attributes).map(|(key, value)| (key.to_string(), value.to_string())).collect(),
            shareable,
        };
        self.find_or_push(hasher.finish(), same, make)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_tree(document: &Document, index: &StyleIndex, cache: &mut StyleCache) {
        fn walk(document: &Document, index: &StyleIndex, cache: &mut StyleCache, node: NodeId, parent: Option<Style>) {
            let style = compute_node_cached(document, node, parent.as_ref(), index, cache).unwrap();
            let mut child = document.first_child(node).unwrap();
            while let Some(current) = child {
                if matches!(document.kind(current), Ok(NodeKind::Element { .. })) {
                    walk(document, index, cache, current, Some(style.clone()));
                }
                child = document.next_sibling(current).unwrap();
            }
        }
        walk(document, index, cache, crate::selector::document_element(document).unwrap(), None);
    }

    fn uncached(document: &Document, index: &StyleIndex, node: NodeId) -> Style {
        let mut chain = alloc::vec![node];
        while let Some(parent) = document.parent(*chain.last().unwrap()).unwrap() {
            if !matches!(document.kind(parent), Ok(NodeKind::Element { .. })) { break; }
            chain.push(parent);
        }
        let mut style: Option<Style> = None;
        for node in chain.into_iter().rev() {
            style = Some(compute_node(document, node, style.as_ref(), index).unwrap());
        }
        style.unwrap()
    }

    fn nodes(document: &Document, selector: &str) -> Vec<NodeId> {
        crate::selector::query_selector_all(document, document.root(), selector).unwrap()
    }

    /// Styles every element through one cache, checks each against an
    /// uncached computation and returns the cache and the nodes of `selector`.
    fn run(html: &str, css: &str, selector: &str, document_hook: impl Fn(&mut Document)) -> (Document, StyleCache, Vec<NodeId>) {
        let mut document = crate::html::parse(html, 64).unwrap();
        document_hook(&mut document);
        let index = StyleIndex::new(parse(css).unwrap());
        let mut cache = StyleCache::default();
        style_tree(&document, &index, &mut cache);
        for node in nodes(&document, "*") {
            assert!(*cache.node_style(node).unwrap() == uncached(&document, &index, node), "{node:?}");
        }
        let selected = nodes(&document, selector);
        (document, cache, selected)
    }

    fn shares(cache: &StyleCache, a: NodeId, b: NodeId) -> bool {
        Arc::ptr_eq(&cache.node_style(a).unwrap(), &cache.node_style(b).unwrap())
    }

    #[test]
    fn identical_siblings_and_cousins_share_one_style() {
        let html = "<ul><li class=a><span>x</span></li><li class=a><span>y</span></li></ul><ul><li class=a><span>z</span></li></ul>";
        let (_, cache, spans) = run(html, ".a span{color:red}", "span", |_| {});
        assert!(shares(&cache, spans[0], spans[1]));
        assert!(shares(&cache, spans[0], spans[2]));
        assert!(cache.cousin_hits >= 2);
    }

    #[test]
    fn untested_attributes_and_ids_do_not_prevent_sharing() {
        let html = "<div><p id=a data-y=1 href=x>a</p><p id=b data-y=2 href=y>b</p></div>";
        let (_, cache, paragraphs) = run(html, "p{color:red}", "p", |_| {});
        assert!(shares(&cache, paragraphs[0], paragraphs[1]));
    }

    #[test]
    fn differing_inputs_prevent_sharing() {
        let cases = [
            ("<div><p class=a></p><p class=b></p></div>", ".a{color:red}.b{color:blue}"),
            ("<div><p data-x=1></p><p data-x=2></p></div>", "[data-x='1']{color:red}"),
            ("<div><p id=a></p><p id=b></p></div>", "#a{color:red}"),
            ("<div><p style='color:red'></p><p style='color:blue'></p></div>", ""),
            ("<div><p></p><p></p><p></p></div>", "p:nth-child(2){color:red}"),
            ("<div class=a><p></p></div><div class=b><p></p></div>", ".a p{color:red}"),
        ];
        for (html, css) in cases {
            let (_, cache, paragraphs) = run(html, css, "p", |_| {});
            let distinct = (1..paragraphs.len()).any(|at| !shares(&cache, paragraphs[0], paragraphs[at]));
            assert!(distinct, "{html} {css}");
        }
    }

    #[test]
    fn position_rules_share_between_elements_that_match_alike() {
        let (_, cache, paragraphs) = run("<div><p></p><p></p><p></p></div>", "p:nth-child(2){color:red}", "p", |_| {});
        assert!(shares(&cache, paragraphs[0], paragraphs[2]));
        assert!(!shares(&cache, paragraphs[0], paragraphs[1]));
    }

    #[test]
    fn interaction_state_is_revalidated_per_element() {
        let hook = |document: &mut Document| {
            let anchors = nodes(document, "a");
            document.set_interaction_state(crate::interaction::InteractionState { hover: Some(anchors[1]), ..Default::default() });
        };
        let (_, cache, anchors) = run("<nav><a>1</a><a>2</a><a>3</a></nav>", "a:hover{color:red}", "a", hook);
        assert!(shares(&cache, anchors[0], anchors[2]));
        assert!(!shares(&cache, anchors[0], anchors[1]));
    }

    #[test]
    fn cousins_under_differently_hovered_ancestors_do_not_share() {
        let hook = |document: &mut Document| {
            let cards = nodes(document, ".card");
            document.set_interaction_state(crate::interaction::InteractionState { hover: Some(cards[0]), ..Default::default() });
        };
        let html = "<main><section class=card><b>1</b></section><section class=card><b>2</b></section><section class=card><b>3</b></section></main>";
        let (_, cache, bold) = run(html, ".card:hover b{color:red}", "b", hook);
        assert!(!shares(&cache, bold[0], bold[1]));
        assert!(shares(&cache, bold[1], bold[2]));
    }

    #[test]
    fn print_type_sizes_css() {
        extern crate std;
        use core::mem::{align_of, size_of};
        macro_rules! sizes {
            ($($ty:ty),+ $(,)?) => {
                $(std::println!("{:<58} size {:>5}  align {}", stringify!($ty), size_of::<$ty>(), align_of::<$ty>());)+
            };
        }
        std::println!("--- css-private: cascade values, caches, rule index");
        sizes!(Value, Declaration, Rule, Selector, AttributeSelector, StructuralPseudo, LogicalPseudo, RelativeExpression, RelativeLength);
        sizes!(StyleCache, MatchedEntry, MatchedCache, MatchedScalars, ParentKey, SharedStyle, SelectedStyleEntry, PseudoStyleEntry);
        sizes!(Option<(NodeId, u32, BoxParentContext, Arc<Style>)>, (u32, AncestorBloom), AncestorBloom, ShareSig, ShareClasses);
        sizes!(SelectorCandidate, KeySlot, KeyedCandidates, UniversalSplit, RuleScan, ShareKeys);
        std::println!(
            "rule index bytes per rule: mask {} + candidate {} (+ key slot {}), kept in each RuleFragment and again in the merged StyleIndex",
            size_of::<AncestorBloom>(), size_of::<SelectorCandidate>(), size_of::<KeySlot>()
        );
        std::println!(
            "StyleCache per-node vectors: nodes {} + previous_nodes {} + blooms {} + viewport_flags 1 + ShareClasses.nodes {} = {} B/node/cache",
            size_of::<Option<(NodeId, u32, BoxParentContext, Arc<Style>)>>(),
            size_of::<Option<(NodeId, u32, BoxParentContext, Arc<Style>)>>(),
            size_of::<(u32, AncestorBloom)>(),
            size_of::<(u32, u32)>(),
            2 * size_of::<Option<(NodeId, u32, BoxParentContext, Arc<Style>)>>() + size_of::<(u32, AncestorBloom)>() + 1 + size_of::<(u32, u32)>()
        );
    }
}
