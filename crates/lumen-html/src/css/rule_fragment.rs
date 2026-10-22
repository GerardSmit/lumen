//! The part of a style index that depends only on one sheet's rules: ancestor
//! masks, summary flags and the selector candidate tables, all counted from
//! the sheet's first rule. A host builds it where the sheet is parsed (any
//! thread); installing the sheet then shifts and merges the tables instead of
//! hashing and sorting every rule again.

use super::{
    index_candidates, AncestorBloom, KeyedCandidates, Rule, RuleFlags, RuleScan, SelectorCandidate, UniversalSplit,
    ascii_ci_cmp,
};
use alloc::{sync::Arc, vec::Vec};

/// Per-sheet index data of [`ParsedStylesheets`](super::ParsedStylesheets).
pub struct RuleFragment {
    len: usize,
    scan: RuleScan,
    tags: KeyedCandidates,
    ids: KeyedCandidates,
    classes: KeyedCandidates,
    universal: UniversalSplit,
    registrations: Vec<u32>,
}

impl core::fmt::Debug for RuleFragment {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_struct("RuleFragment").field("len", &self.len).finish_non_exhaustive()
    }
}

/// A fragment and where its rules start among the rules being indexed.
#[derive(Clone, Debug)]
pub(crate) struct RuleSegment {
    pub(crate) start: usize,
    pub(crate) fragment: Arc<RuleFragment>,
}

impl RuleFragment {
    pub(super) fn build(rules: &[Rule]) -> Self {
        let scan = RuleScan::of(rules);
        let (tags, ids, classes, universal) = index_candidates(rules, &scan.masks, 0);
        let registrations = rules.iter().enumerate().filter(|(_, rule)| rule.registration.is_some()).map(|(order, _)| order as u32).collect();
        Self { len: rules.len(), scan, tags, ids, classes, universal, registrations }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// A rough count of the heap the fragment keeps alive.
    pub(super) fn heap_bytes(&self) -> usize {
        let table = |table: &KeyedCandidates| {
            table.candidates.len() * core::mem::size_of::<SelectorCandidate>()
                + table.slots.len() * core::mem::size_of::<super::KeySlot>()
                + table.keys.len()
        };
        let universal = &self.universal;
        let split = [&universal.attributes].into_iter().chain(universal.alternatives.iter()).map(table).sum::<usize>()
            + (universal.root.len() + universal.state.len() + universal.pseudo.len() + universal.rest.len()) * core::mem::size_of::<SelectorCandidate>();
        self.scan.masks.len() * core::mem::size_of::<AncestorBloom>()
            + table(&self.tags)
            + table(&self.ids)
            + table(&self.classes)
            + split
            + (self.scan.quote_rules.len() + self.scan.pseudo_rules.len()) * core::mem::size_of::<usize>()
            + self.registrations.len() * 4
    }
}

impl super::ParsedStylesheets {
    /// Adds the sheet-local index data, so installing the rules later does not
    /// compute it on the installing thread. Idempotent.
    pub fn with_rule_index(mut self) -> Self {
        if self.rule_index.is_none() && !self.rules.is_empty() && self.rules.iter().all(|rule| rule.scope.is_none()) {
            self.rule_index = Some(Arc::new(RuleFragment::build(&self.rules)));
        }
        self
    }

    /// The index data of the rules as parsed, when they are still the ones it
    /// was built from.
    pub(crate) fn take_rule_index(&mut self) -> Option<Arc<RuleFragment>> {
        self.rule_index.take().filter(|fragment| fragment.len == self.rules.len())
    }
}

enum Part<'a> {
    Shared(&'a RuleFragment),
    Built(RuleFragment),
}

impl Part<'_> {
    fn get(&self) -> &RuleFragment {
        match self {
            Self::Shared(fragment) => fragment,
            Self::Built(fragment) => fragment,
        }
    }
}

/// What `RuleScan::of` and `index_candidates` give for a rule list, with
/// candidate orders counted from `offset`; `quote_rules`, `pseudo_rules` and
/// `registrations` stay relative to the first rule.
pub(super) struct CombinedRules {
    pub(super) flags: RuleFlags,
    pub(super) quote_rules: Vec<usize>,
    pub(super) pseudo_rules: Vec<usize>,
    pub(super) masks: Vec<AncestorBloom>,
    pub(super) siblings_share: bool,
    pub(super) children_state_dependent: bool,
    pub(super) tags: KeyedCandidates,
    pub(super) ids: KeyedCandidates,
    pub(super) classes: KeyedCandidates,
    pub(super) universal: UniversalSplit,
    pub(super) registrations: Vec<u32>,
}

impl CombinedRules {
    /// `segments` are ascending fragments of non-overlapping stretches of
    /// `rules`; the stretches between them are scanned here. A segment that
    /// does not fit is ignored.
    pub(super) fn of(rules: &[Rule], segments: &[RuleSegment], offset: usize) -> Self {
        let mut parts: Vec<(usize, Part<'_>)> = Vec::with_capacity(segments.len() * 2 + 1);
        let mut cursor = 0;
        for segment in segments {
            let len = segment.fragment.len;
            if len == 0 || segment.start < cursor || segment.start + len > rules.len() {
                continue;
            }
            if segment.start > cursor {
                parts.push((cursor, Part::Built(RuleFragment::build(&rules[cursor..segment.start]))));
            }
            parts.push((segment.start, Part::Shared(&segment.fragment)));
            cursor = segment.start + len;
        }
        if cursor < rules.len() {
            parts.push((cursor, Part::Built(RuleFragment::build(&rules[cursor..]))));
        }
        if parts.len() == 1 && matches!(parts[0].1, Part::Built(_)) {
            if let Some((_, Part::Built(fragment))) = parts.pop() {
                return Self::from_fragment(fragment, offset);
            }
        }
        Self::merge(&parts, rules.len(), offset)
    }

    fn from_fragment(fragment: RuleFragment, offset: usize) -> Self {
        let RuleFragment { scan, mut tags, mut ids, mut classes, mut universal, registrations, .. } = fragment;
        let RuleScan { flags, quote_rules, pseudo_rules, masks, siblings_share, children_state_dependent } = scan;
        tags.shift_orders(offset as u32);
        ids.shift_orders(offset as u32);
        classes.shift_orders(offset as u32);
        universal.shift_orders(offset as u32);
        Self { flags, quote_rules, pseudo_rules, masks, siblings_share, children_state_dependent, tags, ids, classes, universal, registrations }
    }

    fn merge(parts: &[(usize, Part<'_>)], total: usize, offset: usize) -> Self {
        let mut flags = RuleFlags {
            has_typographic_content: false,
            transition_pseudo_rules: 0,
            has_z_index: false,
            has_generated_content: false,
            has_quote_content: false,
            has_counter_data: false,
            has_list_item_data: false,
            has_validity_data: false,
        };
        let mut quote_rules = Vec::new();
        let mut pseudo_rules = Vec::new();
        let mut registrations = Vec::new();
        let mut masks = Vec::with_capacity(total);
        let (mut siblings_share, mut children_state_dependent) = (true, false);
        for (start, part) in parts {
            let scan = &part.get().scan;
            let other = scan.flags;
            flags.has_typographic_content |= other.has_typographic_content;
            flags.transition_pseudo_rules |= other.transition_pseudo_rules;
            flags.has_z_index |= other.has_z_index;
            flags.has_generated_content |= other.has_generated_content;
            flags.has_counter_data |= other.has_counter_data;
            flags.has_list_item_data |= other.has_list_item_data;
            flags.has_validity_data |= other.has_validity_data;
            quote_rules.extend(scan.quote_rules.iter().map(|order| start + order));
            pseudo_rules.extend(scan.pseudo_rules.iter().map(|order| start + order));
            registrations.extend(part.get().registrations.iter().map(|order| *start as u32 + order));
            masks.extend_from_slice(&scan.masks);
            siblings_share &= scan.siblings_share;
            children_state_dependent |= scan.children_state_dependent;
        }
        flags.has_quote_content = !quote_rules.is_empty();
        let tags = KeyedCandidates::merge_shifted(&tables(parts, offset, |fragment| &fragment.tags), true);
        let ids = KeyedCandidates::merge_shifted(&tables(parts, offset, |fragment| &fragment.ids), false);
        let classes = KeyedCandidates::merge_shifted(&tables(parts, offset, |fragment| &fragment.classes), false);
        let universal_parts: Vec<(&UniversalSplit, u32)> = parts.iter().map(|(start, part)| (&part.get().universal, (offset + start) as u32)).collect();
        let universal = UniversalSplit::merge_shifted(&universal_parts);
        Self { flags, quote_rules, pseudo_rules, masks, siblings_share, children_state_dependent, tags, ids, classes, universal, registrations }
    }
}

fn tables<'a>(parts: &'a [(usize, Part<'_>)], offset: usize, pick: impl Fn(&RuleFragment) -> &KeyedCandidates) -> Vec<(&'a KeyedCandidates, u32)> {
    parts.iter().map(|(start, part)| (pick(part.get()), (offset + start) as u32)).collect()
}

impl KeyedCandidates {
    fn shift_orders(&mut self, offset: u32) {
        if offset != 0 {
            for candidate in &mut self.candidates {
                candidate.order += offset;
            }
        }
    }

    /// The table `new` would build over the entries of all `parts` together,
    /// each part's candidate orders raised by its offset; the parts must
    /// follow each other in rule order.
    fn merge_shifted(parts: &[(&Self, u32)], fold: bool) -> Self {
        let mut groups: Vec<(u64, &str, &[SelectorCandidate], u32)> = Vec::new();
        let mut candidate_count = 0;
        for (table, offset) in parts {
            candidate_count += table.candidates.len();
            groups.extend(table.sorted_groups().into_iter().map(|(hash, key, candidates)| (hash, key, candidates, *offset)));
        }
        groups.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| if fold { ascii_ci_cmp(a.1, b.1) } else { a.1.cmp(b.1) }));
        let same = |a: &str, b: &str| if fold { a.eq_ignore_ascii_case(b) } else { a == b };
        let mut candidates = Vec::with_capacity(candidate_count);
        let mut placed = Vec::with_capacity(groups.len());
        let mut at = 0;
        while at < groups.len() {
            let (hash, key, _, _) = groups[at];
            let start = candidates.len();
            let mut end = at;
            while end < groups.len() && groups[end].0 == hash && same(groups[end].1, key) {
                let (_, _, members, offset) = groups[end];
                candidates.extend(members.iter().map(|candidate| SelectorCandidate { order: candidate.order + offset, ..*candidate }));
                end += 1;
            }
            placed.push((hash, key, start, candidates.len() - start));
            at = end;
        }
        Self::assemble(fold, placed, candidates)
    }
}

impl UniversalSplit {
    fn shift_orders(&mut self, offset: u32) {
        if offset == 0 {
            return;
        }
        for list in [&mut self.root, &mut self.state, &mut self.pseudo, &mut self.rest] {
            for candidate in list.iter_mut() {
                candidate.order += offset;
            }
        }
        self.attributes.shift_orders(offset);
        for table in &mut self.alternatives {
            table.shift_orders(offset);
        }
    }

    /// The split of all `parts` together, as `merge_later` would build it.
    fn merge_shifted(parts: &[(&Self, u32)]) -> Self {
        let mut merged = Self::default();
        let join = |pick: fn(&UniversalSplit) -> &Vec<SelectorCandidate>| -> Vec<SelectorCandidate> {
            let mut joined = Vec::with_capacity(parts.iter().map(|(part, _)| pick(part).len()).sum());
            for (part, offset) in parts {
                joined.extend(pick(part).iter().map(|candidate| SelectorCandidate { order: candidate.order + offset, ..*candidate }));
            }
            joined
        };
        merged.root = join(|split| &split.root);
        merged.state = join(|split| &split.state);
        merged.pseudo = join(|split| &split.pseudo);
        merged.rest = join(|split| &split.rest);
        let attributes: Vec<(&KeyedCandidates, u32)> = parts.iter().map(|(part, offset)| (&part.attributes, *offset)).collect();
        merged.attributes = KeyedCandidates::merge_shifted(&attributes, true);
        for (kind, table) in merged.alternatives.iter_mut().enumerate() {
            let tables: Vec<(&KeyedCandidates, u32)> = parts.iter().map(|(part, offset)| (&part.alternatives[kind], *offset)).collect();
            *table = KeyedCandidates::merge_shifted(&tables, true);
        }
        merged.has_alternatives = parts.iter().any(|(part, _)| part.has_alternatives);
        merged.has_attribute_alternatives = parts.iter().any(|(part, _)| part.has_attribute_alternatives);
        merged
    }
}
