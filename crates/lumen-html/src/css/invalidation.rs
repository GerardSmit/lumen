//! Invalidation sets: from the selectors of a style index, which elements a
//! DOM change can restyle (the approach of Blink's invalidation sets and
//! Servo's invalidation map). Every class, id and attribute name a selector
//! mentions is recorded with the positions it takes relative to the subject:
//! a change to that name on an element restyles the element, its following
//! siblings, its descendants or its relatives accordingly. Tree changes use
//! the positions of structural pseudo-classes, sibling combinators, `:empty`
//! and `:has()`.

use super::{LogicalPseudo, Relation, RelativeSelector, Rule, Selector, StructuralPseudo};
use crate::{Document, Mutation, MutationKind, NodeId, NodeKind};
use alloc::{collections::BTreeMap, collections::BTreeSet, string::String, vec::Vec};

/// The element itself.
pub(crate) const SELF: u8 = 1;
/// The element's following siblings.
pub(crate) const SIBLINGS: u8 = 2;
/// The element's descendants.
pub(crate) const SUBTREE: u8 = 4;
/// The element's following siblings and their descendants.
pub(crate) const SIBLING_SUBTREES: u8 = 8;
/// The element's ancestors and the siblings of the element and of each
/// ancestor: the anchors a `:has()` argument can be relative to.
pub(crate) const RELATIVES: u8 = 16;
/// Not bounded by the element's tree position.
pub(crate) const DOCUMENT: u8 = 32;
/// The element's relatives (as `RELATIVES`) that are the anchor of a
/// `:has()` outside the subject compound, and what each such anchor reaches
/// (see `InvalidationSets::anchors`).
pub(crate) const ANCHORED: u8 = 64;
/// The element and its ancestors: what the validity of a form or fieldset
/// depends on.
pub(crate) const ANCESTORS: u8 = 128;

/// Bound on the anchor and sibling-key lists; beyond it tree changes are not
/// bounded by tree position.
const MAX_KEYED_ENTRIES: usize = 256;

#[derive(Clone, Copy, PartialEq)]
enum Position {
    Subject,
    Sibling,
    Ancestor,
    SiblingAncestor,
    Relative,
    /// Inside the argument of a `:has()` whose compound is not the subject.
    Anchored,
    Document,
}

impl Position {
    fn reach(self) -> u8 {
        match self {
            Self::Subject => SELF,
            Self::Sibling => SIBLINGS,
            Self::Ancestor => SUBTREE,
            Self::SiblingAncestor => SIBLING_SUBTREES,
            Self::Relative => RELATIVES,
            Self::Anchored => ANCHORED,
            Self::Document => DOCUMENT,
        }
    }

    /// The position of the compound that `relation` leads to from a compound
    /// at this position, seen from the subject.
    fn through(self, relation: &Relation) -> Self {
        match (self, relation) {
            (Self::Relative | Self::Anchored | Self::Document, _) => self,
            (_, Relation::Child | Relation::Descendant) => Self::Ancestor,
            (Self::Subject | Self::Sibling, Relation::Adjacent | Relation::Following) => Self::Sibling,
            (_, Relation::Adjacent | Relation::Following) => Self::SiblingAncestor,
        }
    }
}

/// Attributes whose writes change element state that pseudo-classes read
/// (`:checked`, `:disabled`, `:link`, `:lang()`, `:open`, ...).
fn state_attribute(name: &str) -> bool {
    matches!(
        name,
        "checked" | "selected" | "disabled" | "readonly" | "required" | "value" | "type" | "multiple"
            | "min" | "max" | "step" | "pattern" | "minlength" | "maxlength" | "placeholder"
            | "name" | "form" | "for" | "list" | "href" | "open" | "popover" | "dir" | "lang"
            | "xml:lang" | "contenteditable" | "inert" | "hidden" | "autofocus" | "indeterminate"
            | "default" | "label" | "src" | "srcdoc"
    )
}

/// The conditions on an element that every element a subject compound matches
/// has: one of the ids, classes, tags or attribute names. `whole` stands for
/// a compound that does not name any.
#[derive(Default, Clone, Debug, PartialEq)]
struct Features {
    whole: bool,
    ids: BTreeSet<String>,
    classes: BTreeSet<String>,
    /// ASCII-lowercase.
    tags: BTreeSet<String>,
    /// ASCII-lowercase.
    attributes: BTreeSet<String>,
}

impl Features {
    /// The features of the subject compound of `selector`.
    fn of_subject(selector: &Selector) -> Self {
        let mut features = Self::default();
        features.whole = !features.require(selector);
        features
    }

    /// Record a necessary feature of `selector`'s compound; false when none
    /// is known.
    fn require(&mut self, selector: &Selector) -> bool {
        if selector.host || selector.slotted || selector.part.is_some() {
            return false;
        }
        if let Some(id) = &selector.id {
            self.ids.insert(id.clone());
        } else if let Some(class) = selector.classes.first() {
            self.classes.insert(class.clone());
        } else if let Some(attribute) = selector.attributes.first() {
            self.attributes.insert(attribute.name.to_ascii_lowercase());
        } else if let Some(tag) = selector.tag.as_ref().filter(|tag| tag.as_str() != "*") {
            self.tags.insert(tag.to_ascii_lowercase());
        } else {
            return selector.logical.iter().any(|pseudo| {
                let list: &[Selector] = match pseudo {
                    LogicalPseudo::Is(list) | LogicalPseudo::Where(list) => list,
                    LogicalPseudo::Nesting(list) => list,
                    _ => return false,
                };
                !list.is_empty() && list.iter().all(|alternative| self.require(alternative))
            });
        }
        true
    }

    fn merge(&mut self, other: &Self) {
        self.whole |= other.whole;
        self.ids.extend(other.ids.iter().cloned());
        self.classes.extend(other.classes.iter().cloned());
        self.tags.extend(other.tags.iter().cloned());
        self.attributes.extend(other.attributes.iter().cloned());
    }

    fn matches(&self, document: &Document, node: NodeId) -> bool {
        let Ok(NodeKind::Element { name, attributes, .. }) = document.kind(node) else { return false };
        if !self.tags.is_empty() && self.tags.iter().any(|tag| crate::svg::local_name(name).eq_ignore_ascii_case(tag)) {
            return true;
        }
        attributes.iter().any(|(key, value)| {
            let key = key.as_str();
            let value: &str = &**value;
            (!self.ids.is_empty() && key.eq_ignore_ascii_case("id") && self.ids.contains(value))
                || (!self.classes.is_empty()
                    && key.eq_ignore_ascii_case("class")
                    && value.split_ascii_whitespace().any(|class| self.classes.contains(class)))
                || (!self.attributes.is_empty() && self.attributes.iter().any(|wanted| local_part(wanted).eq_ignore_ascii_case(local_part(key))))
        })
    }
}

/// What an attribute write reaches beyond a plain `reach` mask: descendants
/// narrowed to the subject features of the selectors that read the changed
/// name, and the subtree of a form.
#[derive(Default)]
struct Spread<'a> {
    features: Vec<&'a Features>,
    /// The element's descendants match one of the features.
    subtree: bool,
    /// The descendants of the element's following siblings match one.
    siblings: bool,
    /// An element whose whole subtree is restyled.
    scope: Option<NodeId>,
}

impl<'a> Spread<'a> {
    /// `reach` of a key with its descendant reach narrowed to `subjects`
    /// when those are known.
    fn narrow(&mut self, reach: u8, subjects: Option<&'a Features>) -> u8 {
        let wide = reach & (SUBTREE | SIBLING_SUBTREES);
        match subjects {
            Some(features) if wide != 0 && !features.whole => {
                self.features.push(features);
                self.subtree |= wide & SUBTREE != 0;
                self.siblings |= wide & SIBLING_SUBTREES != 0;
                reach & !wide
            }
            _ => reach,
        }
    }

    fn is_empty(&self) -> bool {
        self.features.is_empty() && self.scope.is_none()
    }
}

fn local_part(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

/// Whether `node` is in the document tree, shadow trees included.
fn connected(document: &Document, node: NodeId) -> bool {
    let mut current = node;
    loop {
        if current == document.root() {
            return true;
        }
        let Ok(Some(parent)) = document.shadow_including_parent(current) else { return false };
        current = parent;
    }
}

/// The nearest `<form>` ancestor of `node`.
fn form_owner(document: &Document, node: NodeId) -> Option<NodeId> {
    let mut current = document.parent(node).ok().flatten();
    while let Some(id) = current {
        if crate::forms::html_element_local_name(document, id) == Some("form") {
            return Some(id);
        }
        current = document.parent(id).ok().flatten();
    }
    None
}

/// Form-associated elements, whose state other elements also read (radio
/// groups, a form's or fieldset's validity, a label's control).
fn form_element(document: &Document, node: NodeId) -> bool {
    matches!(
        crate::forms::html_element_local_name(document, node),
        Some("input" | "select" | "option" | "optgroup" | "textarea" | "button" | "fieldset" | "form" | "label" | "output" | "datalist")
    )
}

#[derive(Default)]
pub(crate) struct InvalidationSets {
    classes: BTreeMap<String, u8>,
    ids: BTreeMap<String, u8>,
    /// ASCII-lowercase attribute names.
    attributes: BTreeMap<String, u8>,
    any_class: u8,
    /// Features of the subject compounds reached through a key in an ancestor
    /// position, per key.
    class_subjects: BTreeMap<String, Features>,
    id_subjects: BTreeMap<String, Features>,
    attribute_subjects: BTreeMap<String, Features>,
    any_class_subjects: Features,
    any_id: u8,
    any_id_subjects: Features,
    /// Positions of pseudo-classes reading state that attribute writes change.
    state: u8,
    /// Positions of structural pseudo-classes and of compounds reached
    /// through sibling combinators: a child list change moves siblings.
    siblings: u8,
    /// Positions of `:empty`, `:has()` and container validity: a child list
    /// change can change them for the parent and its relatives.
    children: u8,
    /// Positions of `:empty`: a text node gaining or losing data changes it.
    empty: u8,
    /// `@scope` roots and limits are matched against ancestors.
    scoped: bool,
    /// Per `:has()` outside a subject compound: a key its anchor compound
    /// requires, and what a change of the anchor's match reaches from it.
    anchors: Vec<(AnchorKey, u8)>,
    /// Per compound with a key whose match reads its siblings (structural
    /// pseudo-classes, sibling combinators): the key and the compound's
    /// reach. A child list change restyles the children with that key.
    sibling_keys: Vec<(AnchorKey, u8, SiblingEdge)>,
    /// Positions of unkeyed `:first-child`, `:last-child` and `:only-child`:
    /// a child list change restyles only the first or last two children.
    first_edge: u8,
    last_edge: u8,
    /// Subjects below a compound that reads its siblings: a child list
    /// change restyles only these in the subtrees that compound reaches.
    structural_subjects: Features,
    /// Per `:has()` in a subject compound: a key the subject requires and
    /// whether an argument looks at following siblings.
    relative_keys: Vec<(AnchorKey, bool)>,
    /// A subject `:has()` has no key, or there are too many: a change reaches
    /// all its relatives.
    unkeyed_relatives: bool,
}

/// The subject keys of `:has()` arguments, as seen by a `Collector`.
#[derive(Clone, Copy)]
struct Relatives<'a> {
    keys: &'a [(AnchorKey, bool)],
    unkeyed: bool,
    /// Siblings of an element can be anchors.
    siblings: bool,
}

/// Which children a structural pseudo-class's match depends on the order of.
#[derive(Clone, Copy, PartialEq)]
enum SiblingEdge {
    First,
    Last,
    Both,
    All,
}

/// The id, a class or the tag every element an anchor compound matches has.
#[derive(Clone, PartialEq)]
enum AnchorKey {
    Id(String),
    Class(String),
    Tag(String),
}

impl AnchorKey {
    fn of(selector: &Selector) -> Option<Self> {
        if let Some(id) = &selector.id {
            return Some(Self::Id(id.clone()));
        }
        if let Some(class) = selector.classes.first() {
            return Some(Self::Class(class.clone()));
        }
        selector.tag.as_ref().filter(|tag| tag.as_str() != "*").map(|tag| Self::Tag(tag.to_ascii_lowercase()))
    }

    fn matches(&self, document: &Document, node: NodeId) -> bool {
        let Ok(NodeKind::Element { name, attributes, .. }) = document.kind(node) else { return false };
        let attribute = |wanted: &str| attributes.iter().find(|(key, _)| key.as_str().eq_ignore_ascii_case(wanted)).map(|(_, value)| &**value);
        match self {
            Self::Id(id) => attribute("id") == Some(id.as_str()),
            Self::Class(class) => attribute("class").is_some_and(|classes| classes.split_ascii_whitespace().any(|other| other == class)),
            Self::Tag(tag) => crate::svg::local_name(name).eq_ignore_ascii_case(tag),
        }
    }
}

impl InvalidationSets {
    pub(crate) fn new(rules: &[Rule]) -> Self {
        let mut sets = Self::default();
        sets.extend(rules);
        sets
    }

    /// Add the selectors of rules appended after the ones already recorded.
    pub(crate) fn extend(&mut self, rules: &[Rule]) {
        for rule in rules {
            self.scoped |= rule.css_scope.is_some();
            let subject = Features::of_subject(&rule.selector);
            self.add(&rule.selector, Position::Subject, &subject);
        }
        self.any_class = self.classes.values().fold(0, |reach, value| reach | value);
        self.any_id = self.ids.values().fold(0, |reach, value| reach | value);
        let union = |sets: &BTreeMap<String, Features>| {
            let mut all = Features::default();
            for features in sets.values() {
                all.merge(features);
            }
            all
        };
        self.any_class_subjects = union(&self.class_subjects);
        self.any_id_subjects = union(&self.id_subjects);
    }

    /// Record the selectors of one rule. `subject` holds the features of the
    /// rule's subject compound, which a change in an ancestor position
    /// reaches.
    fn add(&mut self, selector: &Selector, position: Position, subject: &Features) {
        // `:host`, `::slotted()` and `::part()` cross tree scopes.
        let position = if selector.host || selector.slotted || selector.part.is_some() { Position::Document } else { position };
        let reach = position.reach();
        let wide = reach & (SUBTREE | SIBLING_SUBTREES) != 0;
        for class in &selector.classes {
            *self.classes.entry(class.clone()).or_default() |= reach;
            if wide {
                self.class_subjects.entry(class.clone()).or_default().merge(subject);
            }
        }
        if let Some(id) = &selector.id {
            *self.ids.entry(id.clone()).or_default() |= reach;
            if wide {
                self.id_subjects.entry(id.clone()).or_default().merge(subject);
            }
        }
        for attribute in &selector.attributes {
            let name = attribute.name.to_ascii_lowercase();
            if wide {
                self.attribute_subjects.entry(name.clone()).or_default().merge(subject);
            }
            *self.attributes.entry(name).or_default() |= reach;
        }
        if !selector.form_state.is_empty()
            || !selector.interaction_state.is_empty()
            || selector.top_layer_state != 0
            || selector.directionality_mask != 0
            || !selector.languages.is_empty()
        {
            self.state |= reach;
        }
        for pseudo in &selector.structural {
            match pseudo {
                StructuralPseudo::Empty => {
                    self.children |= reach;
                    self.empty |= reach;
                }
                StructuralPseudo::Valid | StructuralPseudo::Invalid => {
                    self.children |= ANCESTORS;
                    self.state |= ANCESTORS;
                }
                StructuralPseudo::Nth { a, b, of_type, reverse, of } => {
                    let edge = match (*a, *b, *of_type, *reverse, of.is_none()) {
                        (0, 1, false, false, true) => SiblingEdge::First,
                        (0, 1, false, true, true) => SiblingEdge::Last,
                        _ => SiblingEdge::All,
                    };
                    self.sibling(selector, position, edge, subject);
                    for inner in of.iter().flatten() {
                        self.add(inner, position, subject);
                        self.add(inner, position.through(&Relation::Following), subject);
                    }
                }
                StructuralPseudo::Last(of_type) => self.sibling(selector, position, if *of_type { SiblingEdge::All } else { SiblingEdge::Last }, subject),
                StructuralPseudo::Only(of_type) => self.sibling(selector, position, if *of_type { SiblingEdge::All } else { SiblingEdge::Both }, subject),
            }
        }
        for pseudo in &selector.logical {
            match pseudo {
                LogicalPseudo::Not(list) | LogicalPseudo::Is(list) | LogicalPseudo::Where(list) => {
                    for inner in list {
                        self.add(inner, position, subject);
                    }
                }
                LogicalPseudo::Nesting(list) => {
                    for inner in list.iter() {
                        self.add(inner, position, subject);
                    }
                }
                LogicalPseudo::Has(relatives) => {
                    let relatives: &[RelativeSelector] = relatives;
                    // A `:has()` in another compound changes the match of
                    // that compound only, so it reaches what the compound's
                    // position reaches from each anchor its key admits.
                    let anchored = match position {
                        Position::Ancestor => Some(SUBTREE),
                        Position::Sibling => Some(SIBLINGS),
                        Position::SiblingAncestor => Some(SIBLING_SUBTREES),
                        _ => None,
                    }
                    .and_then(|reach| AnchorKey::of(selector).map(|key| (key, reach)));
                    let inner = match anchored {
                        _ if position == Position::Subject => {
                            self.relative(selector, relatives);
                            Position::Relative
                        }
                        Some(anchor) => {
                            if !self.anchors.contains(&anchor) {
                                if self.anchors.len() < MAX_KEYED_ENTRIES {
                                    self.anchors.push(anchor);
                                } else {
                                    self.children |= DOCUMENT;
                                }
                            }
                            Position::Anchored
                        }
                        None => Position::Document,
                    };
                    self.children |= inner.reach();
                    for relative in relatives {
                        self.add(&relative.selector, inner, subject);
                    }
                }
                LogicalPseudo::State(..) => self.state |= reach,
                LogicalPseudo::Heading(_) | LogicalPseudo::InvalidNesting => {}
            }
        }
        if let Some((relation, ancestor)) = &selector.ancestor {
            let next = position.through(relation);
            if matches!(relation, Relation::Adjacent | Relation::Following) {
                self.sibling(selector, position, SiblingEdge::All, subject);
            }
            self.add(ancestor, next, subject);
        }
    }

    /// Record the key of a subject compound holding a `:has()`.
    fn relative(&mut self, selector: &Selector, relatives: &[RelativeSelector]) {
        let siblings = relatives.iter().any(|relative| {
            let mut compound = Some(&relative.selector);
            let mut sibling = matches!(relative.relation, Relation::Adjacent | Relation::Following);
            while let Some(current) = compound {
                if sibling {
                    return true;
                }
                compound = current.ancestor.as_ref().map(|(relation, next)| {
                    sibling = matches!(relation, Relation::Adjacent | Relation::Following);
                    &**next
                });
            }
            false
        });
        let Some(key) = AnchorKey::of(selector) else {
            self.unkeyed_relatives = true;
            return;
        };
        if let Some((_, known)) = self.relative_keys.iter_mut().find(|(other, _)| *other == key) {
            *known |= siblings;
        } else if self.relative_keys.len() < MAX_KEYED_ENTRIES {
            self.relative_keys.push((key, siblings));
        } else {
            self.unkeyed_relatives = true;
        }
    }

    /// Record that the match of `selector`, a compound at `position`, reads
    /// its siblings.
    fn sibling(&mut self, selector: &Selector, position: Position, edge: SiblingEdge, subject: &Features) {
        if position.reach() & (SUBTREE | SIBLING_SUBTREES) != 0 {
            self.structural_subjects.merge(subject);
        }
        let keyed = match position {
            Position::Subject | Position::Sibling | Position::Ancestor | Position::SiblingAncestor => AnchorKey::of(selector),
            Position::Relative | Position::Anchored | Position::Document => None,
        };
        match keyed {
            Some(key) => {
                let entry = (key, position.reach(), edge);
                if !self.sibling_keys.contains(&entry) {
                    if self.sibling_keys.len() < MAX_KEYED_ENTRIES {
                        self.sibling_keys.push(entry);
                    } else {
                        self.siblings |= DOCUMENT;
                    }
                }
            }
            None => match (edge, position) {
                (_, Position::Relative | Position::Anchored | Position::Document) | (SiblingEdge::All, _) => self.siblings |= position.reach(),
                (SiblingEdge::First, _) => self.first_edge |= position.reach(),
                (SiblingEdge::Last, _) => self.last_edge |= position.reach(),
                (SiblingEdge::Both, _) => {
                    self.first_edge |= position.reach();
                    self.last_edge |= position.reach();
                }
            },
        }
    }

    fn class_reach<'a>(&'a self, old: Option<&str>, new: Option<&str>, spread: &mut Spread<'a>) -> u8 {
        let mut reach = 0;
        let new_classes = || new.unwrap_or("").split_ascii_whitespace();
        match old {
            Some(old) => {
                let old_classes = || old.split_ascii_whitespace();
                for class in old_classes().filter(|class| !new_classes().any(|other| other == *class)) {
                    reach |= self.class_key(class, spread);
                }
                for class in new_classes().filter(|class| !old_classes().any(|other| other == *class)) {
                    reach |= self.class_key(class, spread);
                }
            }
            None => reach |= spread.narrow(self.any_class, Some(&self.any_class_subjects)),
        }
        reach
    }

    fn class_key<'a>(&'a self, class: &str, spread: &mut Spread<'a>) -> u8 {
        self.classes.get(class).map_or(0, |reach| spread.narrow(*reach, self.class_subjects.get(class)))
    }

    fn id_key<'a>(&'a self, id: Option<&str>, spread: &mut Spread<'a>) -> u8 {
        id.and_then(|id| self.ids.get(id).map(|reach| spread.narrow(*reach, self.id_subjects.get(id)))).unwrap_or(0)
    }

    /// What an attribute write on `node` can restyle.
    fn attribute_reach<'a>(&'a self, document: &Document, node: NodeId, name: &str, replaced: Option<Option<&str>>, spread: &mut Spread<'a>) -> u8 {
        let current = |attribute: &str| document.get_attribute_ns_ref(node, None, attribute).ok().flatten();
        let mut reach = SELF;
        match name {
            "class" => reach |= self.class_reach(replaced.map_or(None, |value| Some(value.unwrap_or(""))), current("class"), spread),
            "id" => {
                reach |= self.id_key(current("id"), spread) | self.state;
                reach |= match replaced {
                    Some(old) => self.id_key(old, spread),
                    None => spread.narrow(self.any_id, Some(&self.any_id_subjects)),
                };
            }
            "style" => {}
            _ => {
                let lower = name.to_ascii_lowercase();
                if let Some(keyed) = self.attributes.get(lower.as_str()) {
                    reach |= spread.narrow(*keyed, self.attribute_subjects.get(lower.as_str()));
                }
                if state_attribute(lower.as_str()) {
                    reach |= self.state;
                    if self.state != 0 && form_element(document, node) {
                        reach |= self.form_reach(document, node, lower.as_str(), spread);
                    }
                }
                if matches!(lower.as_str(), "dir" | "lang" | "xml:lang") {
                    reach |= SUBTREE;
                }
            }
        }
        if self.scoped && (reach != SELF || !spread.is_empty()) {
            reach |= SUBTREE;
        }
        reach
    }

    /// What a state attribute write on a form-associated element reaches
    /// besides the element: its form's controls (radio groups, the default
    /// button), the controls a fieldset or group disables, and everything for
    /// associations that cross the tree.
    fn form_reach(&self, document: &Document, node: NodeId, name: &str, spread: &mut Spread<'_>) -> u8 {
        let current = |attribute: &str| document.get_attribute_ns_ref(node, None, attribute).ok().flatten();
        let local = crate::forms::html_element_local_name(document, node);
        // A label's `for` only feeds activation, which no selector reads.
        if matches!(name, "form" | "list") || current("form").is_some() {
            return DOCUMENT;
        }
        let owner = form_owner(document, node);
        let radio = local == Some("input") && (name == "type" || current("type").is_some_and(|kind| kind.eq_ignore_ascii_case("radio")));
        if radio && owner.is_none() {
            return DOCUMENT;
        }
        spread.scope = owner;
        if matches!(local, Some("fieldset" | "form" | "optgroup" | "select" | "datalist")) {
            SUBTREE
        } else {
            0
        }
    }

    /// The elements whose styles the journaled mutations can change, or
    /// `None` when that is not bounded by their tree positions.
    pub(crate) fn affected(&self, document: &Document, mutations: &[Mutation]) -> Option<Vec<NodeId>> {
        let limit = (document.node_count() / 2).clamp(MIN_TARGETS, MAX_TARGETS);
        let relatives = Relatives {
            keys: &self.relative_keys,
            unkeyed: self.unkeyed_relatives,
            siblings: self.unkeyed_relatives || self.relative_keys.iter().any(|(_, siblings)| *siblings),
        };
        let work_limit = limit * 16 + document.node_count() * 8;
        let mut collector = Collector { document, anchors: &self.anchors, relatives, seen: BTreeSet::new(), nodes: Vec::new(), limit, work: 0, work_limit };
        for mutation in mutations {
            if document.kind(mutation.target).is_err() {
                continue;
            }
            match &mutation.kind {
                MutationKind::Attribute(name) => {
                    if !document.is_connected_element(mutation.target) {
                        continue;
                    }
                    let replaced = document.replaced_identity(mutation.version);
                    let mut spread = Spread::default();
                    let reach = self.attribute_reach(document, mutation.target, name.as_str(), replaced, &mut spread);
                    collector.reach(mutation.target, reach)?;
                    collector.spread(mutation.target, &spread)?;
                }
                MutationKind::CharacterData => {
                    if self.empty != 0 {
                        if let Some(parent) = document.parent(mutation.target).ok().flatten() {
                            if connected(document, parent) {
                                collector.reach(parent, self.empty)?;
                            }
                        }
                    }
                }
                MutationKind::Tree { added, removed, .. } => {
                    let parent = mutation.target;
                    if !connected(document, parent) {
                        continue;
                    }
                    if mutation.is_subtree_summary() {
                        if !self.subtree_local() {
                            return None;
                        }
                        collector.reach(parent, SELF | SUBTREE)?;
                        continue;
                    }
                    match (added, removed) {
                        (Some(added), _) => {
                            if document.parent(*added).ok().flatten() == Some(parent) {
                                collector.reach(*added, SELF | SUBTREE)?;
                            }
                        }
                        (None, None) => collector.reach(parent, SUBTREE)?,
                        (None, Some(_)) => {}
                    }
                    if self.siblings != 0 {
                        let children = if self.siblings & (SUBTREE | SIBLING_SUBTREES) != 0 { SELF | SUBTREE } else { SELF };
                        let mut child = document.first_child(parent).ok().flatten();
                        while let Some(node) = child {
                            self.structural_reach(&mut collector, node, children)?;
                            child = document.next_sibling(node).ok().flatten();
                        }
                        if self.siblings & (RELATIVES | ANCHORED | DOCUMENT) != 0 {
                            collector.reach(parent, self.siblings & (RELATIVES | ANCHORED | DOCUMENT))?;
                        }
                    }
                    if self.first_edge | self.last_edge != 0 || !self.sibling_keys.is_empty() {
                        let element = |node: Option<NodeId>, forward: bool| {
                            let mut current = node;
                            while let Some(id) = current {
                                if matches!(document.kind(id), Ok(NodeKind::Element { .. })) {
                                    return Some(id);
                                }
                                current = if forward { document.next_sibling(id) } else { document.previous_sibling(id) }.ok().flatten();
                            }
                            None
                        };
                        // The first or last child before the change is now
                        // the second or second to last, the one removed, or
                        // (when later changes moved the edge further) next to
                        // a node this batch added.
                        let first = element(document.first_child(parent).ok().flatten(), true);
                        let second = first.and_then(|first| element(document.next_sibling(first).ok().flatten(), true));
                        let last = element(document.last_child(parent).ok().flatten(), false);
                        let second_last = last.and_then(|last| element(document.previous_sibling(last).ok().flatten(), false));
                        let inserted = added.filter(|added| document.parent(*added).ok().flatten() == Some(parent));
                        let before = inserted.and_then(|added| element(document.previous_sibling(added).ok().flatten(), false));
                        let after = inserted.and_then(|added| element(document.next_sibling(added).ok().flatten(), true));
                        for node in [before, after].into_iter().flatten() {
                            if self.first_edge | self.last_edge != 0 {
                                self.structural_reach(&mut collector, node, SELF | self.first_edge | self.last_edge)?;
                            }
                        }
                        for node in [first, second].into_iter().flatten() {
                            if self.first_edge != 0 {
                                self.structural_reach(&mut collector, node, SELF | self.first_edge)?;
                            }
                        }
                        for node in [last, second_last].into_iter().flatten() {
                            if self.last_edge != 0 {
                                self.structural_reach(&mut collector, node, SELF | self.last_edge)?;
                            }
                        }
                        let neighbour = |node: NodeId| before == Some(node) || after == Some(node);
                        let at_first = |node: NodeId| first == Some(node) || second == Some(node) || neighbour(node);
                        let at_last = |node: NodeId| last == Some(node) || second_last == Some(node) || neighbour(node);
                        let mut child = first;
                        while let Some(node) = child {
                            for (key, reach, edge) in &self.sibling_keys {
                                let affected = match edge {
                                    SiblingEdge::All => true,
                                    SiblingEdge::First => at_first(node),
                                    SiblingEdge::Last => at_last(node),
                                    SiblingEdge::Both => at_first(node) || at_last(node),
                                };
                                if affected && key.matches(document, node) {
                                    self.structural_reach(&mut collector, node, SELF | *reach)?;
                                }
                            }
                            child = element(document.next_sibling(node).ok().flatten(), true);
                        }
                    }
                    if self.children != 0 {
                        collector.reach(parent, self.children)?;
                        let relatives = self.children & RELATIVES != 0 && collector.relatives.siblings;
                        if relatives || self.children & ANCHORED != 0 {
                            let mut child = document.first_child(parent).ok().flatten();
                            while let Some(node) = child {
                                if relatives {
                                    collector.relative(node, true)?;
                                }
                                if self.children & ANCHORED != 0 {
                                    collector.anchor(node)?;
                                }
                                child = document.next_sibling(node).ok().flatten();
                            }
                        }
                    }
                }
                MutationKind::FullRebuild => return None,
            }
        }
        Some(collector.nodes)
    }

    /// `collector.reach` for a compound whose sibling position changed, with
    /// its descendant reach narrowed to `structural_subjects`.
    fn structural_reach(&self, collector: &mut Collector<'_>, node: NodeId, reach: u8) -> Option<()> {
        let wide = reach & (SUBTREE | SIBLING_SUBTREES);
        if wide == 0 || self.structural_subjects.whole {
            return collector.reach(node, reach);
        }
        collector.reach(node, reach & !wide)?;
        let spread = Spread { features: alloc::vec![&self.structural_subjects], subtree: wide & SUBTREE != 0, siblings: wide & SIBLING_SUBTREES != 0, scope: None };
        collector.spread(node, &spread)
    }

    /// Whether a change anywhere below an element only reaches that element's
    /// subtree: no selector reads state, siblings, children or ancestors'
    /// matches from outside it.
    fn subtree_local(&self) -> bool {
        self.state | self.children | self.siblings | self.first_edge | self.last_edge == 0
            && self.anchors.is_empty()
            && self.sibling_keys.is_empty()
            && !self.scoped
    }
}


/// Targets one batch may restyle before a full restyle is cheaper.
const MIN_TARGETS: usize = 256;
const MAX_TARGETS: usize = 16384;

struct Collector<'a> {
    document: &'a Document,
    anchors: &'a [(AnchorKey, u8)],
    relatives: Relatives<'a>,
    seen: BTreeSet<(u64, u32, u32)>,
    nodes: Vec<NodeId>,
    limit: usize,
    work: usize,
    work_limit: usize,
}

impl Collector<'_> {
    fn tick(&mut self) -> Option<()> {
        self.work += 1;
        (self.work <= self.work_limit).then_some(())
    }

    fn add(&mut self, node: NodeId) -> Option<bool> {
        self.tick()?;
        if !matches!(self.document.kind(node), Ok(NodeKind::Element { .. })) {
            return Some(false);
        }
        if !self.seen.insert((node.document, node.index, node.generation)) {
            return Some(false);
        }
        if self.nodes.len() >= self.limit {
            return None;
        }
        self.nodes.push(node);
        Some(true)
    }

    /// Add the descendants of `node`, or with `filter` those that match one of
    /// its features.
    fn descendants(&mut self, node: NodeId, filter: Option<&[&Features]>) -> Option<()> {
        let mut pending = Vec::new();
        pending.push(node);
        while let Some(current) = pending.pop() {
            let mut child = self.document.first_child(current).ok().flatten();
            while let Some(id) = child {
                if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
                    self.visit(id, filter)?;
                    pending.push(id);
                }
                child = self.document.next_sibling(id).ok().flatten();
            }
            if let Some(root) = self.document.shadow_root(current).ok().flatten() {
                pending.push(root);
            }
            if filter.is_none() && pending.len() > self.limit {
                return None;
            }
        }
        Some(())
    }

    fn visit(&mut self, node: NodeId, filter: Option<&[&Features]>) -> Option<()> {
        match filter {
            None => self.add(node).map(|_| ()),
            Some(features) => {
                self.tick()?;
                if features.iter().any(|set| set.matches(self.document, node)) {
                    self.add(node)?;
                }
                Some(())
            }
        }
    }

    /// Reach what `spread` narrows or adds beyond a `reach` mask.
    fn spread(&mut self, node: NodeId, spread: &Spread<'_>) -> Option<()> {
        if spread.subtree {
            self.descendants(node, Some(&spread.features))?;
        }
        if spread.siblings {
            let mut sibling = self.document.next_sibling(node).ok().flatten();
            while let Some(id) = sibling {
                if matches!(self.document.kind(id), Ok(NodeKind::Element { .. })) {
                    self.visit(id, Some(&spread.features))?;
                    self.descendants(id, Some(&spread.features))?;
                }
                sibling = self.document.next_sibling(id).ok().flatten();
            }
        }
        if let Some(scope) = spread.scope {
            self.reach(scope, SELF | SUBTREE)?;
        }
        Some(())
    }

    fn reach(&mut self, node: NodeId, reach: u8) -> Option<()> {
        if reach & DOCUMENT != 0 {
            return None;
        }
        if reach & SELF != 0 {
            self.add(node)?;
        }
        if reach & SUBTREE != 0 {
            self.descendants(node, None)?;
        }
        if reach & (SIBLINGS | SIBLING_SUBTREES) != 0 {
            let mut sibling = self.document.next_sibling(node).ok().flatten();
            while let Some(id) = sibling {
                self.add(id)?;
                if reach & SIBLING_SUBTREES != 0 {
                    self.descendants(id, None)?;
                }
                sibling = self.document.next_sibling(id).ok().flatten();
            }
        }
        if reach & ANCESTORS != 0 {
            let mut current = Some(node);
            while let Some(id) = current {
                self.add(id)?;
                current = self.document.composed_parent(id).ok().flatten();
            }
        }
        let relatives = reach & RELATIVES != 0;
        let anchored = reach & ANCHORED != 0;
        if relatives || anchored {
            let siblings = anchored || self.relatives.siblings;
            let mut current = Some(node);
            while let Some(id) = current {
                if relatives {
                    self.relative(id, false)?;
                }
                if anchored {
                    self.anchor(id)?;
                }
                if let Some(parent) = self.document.parent(id).ok().flatten().filter(|_| siblings) {
                    let mut sibling = self.document.first_child(parent).ok().flatten();
                    while let Some(other) = sibling {
                        if relatives {
                            self.relative(other, true)?;
                        }
                        if anchored {
                            self.anchor(other)?;
                        }
                        sibling = self.document.next_sibling(other).ok().flatten();
                    }
                }
                current = self.document.composed_parent(id).ok().flatten();
            }
        }
        Some(())
    }

    /// Add `node` when it can be the anchor of a subject `:has()` that a
    /// change at a relative of it reaches: an ancestor, or with `sibling` a
    /// sibling of the changed element or of one of its ancestors.
    fn relative(&mut self, node: NodeId, sibling: bool) -> Option<()> {
        if self.relatives.unkeyed {
            return self.add(node).map(|_| ());
        }
        self.tick()?;
        let relatives = self.relatives;
        if relatives.keys.iter().any(|(key, siblings)| (*siblings || !sibling) && key.matches(self.document, node)) {
            self.add(node)?;
        }
        Some(())
    }

    /// Reach what each anchored `:has()` whose anchor key `node` has reaches.
    fn anchor(&mut self, node: NodeId) -> Option<()> {
        self.tick()?;
        let anchors = self.anchors;
        for (key, reach) in anchors {
            if key.matches(self.document, node) {
                self.reach(node, *reach)?;
            }
        }
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::css::parse_stylesheet;
    use alloc::format;

    fn elements(document: &Document, tag: &str) -> Vec<NodeId> {
        let mut found = Vec::new();
        let mut pending = alloc::vec![document.root()];
        while let Some(node) = pending.pop() {
            if matches!(document.kind(node), Ok(NodeKind::Element { name, .. }) if crate::svg::local_name(name) == tag) {
                found.push(node);
            }
            let mut children = Vec::new();
            let mut child = document.first_child(node).unwrap();
            while let Some(id) = child {
                children.push(id);
                child = document.next_sibling(id).unwrap();
            }
            pending.extend(children.into_iter().rev());
        }
        found
    }

    fn setup(html: &str, css: &str) -> (Document, InvalidationSets) {
        let mut document = crate::html::parse(html, 65536).unwrap();
        document.clear_mutations();
        let rules = parse_stylesheet(css).unwrap().rules;
        (document, InvalidationSets::new(&rules))
    }

    #[test]
    fn class_toggle_reaches_only_the_element() {
        let (mut document, sets) = setup("<div><p>1</p><p>2</p><p>3</p></div>", ".hot { color: red }");
        let middle = elements(&document, "p")[1];
        document.set_attribute(middle, "class", "hot").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), Some(alloc::vec![middle]));
    }

    #[test]
    fn descendant_selector_reaches_the_subtree_only() {
        let (mut document, sets) = setup("<div id=a><section><i>1</i></section></div><i>2</i>", ".a i { color: red }");
        let host = elements(&document, "div")[0];
        let inner = elements(&document, "i")[0];
        let outer = elements(&document, "i")[1];
        document.set_attribute(host, "class", "a").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&host) && affected.contains(&inner));
        assert!(!affected.contains(&outer));
    }

    #[test]
    fn sibling_combinator_reaches_following_siblings() {
        let (mut document, sets) = setup("<p class=a>1</p><p>2</p><p>3</p>", ".a + p { color: red }");
        let paragraphs = elements(&document, "p");
        document.set_attribute(paragraphs[0], "class", "").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(paragraphs.iter().all(|node| affected.contains(node)));
    }

    #[test]
    fn has_reaches_ancestors() {
        let (mut document, sets) = setup("<div id=d><section><p>t</p></section></div><div id=other></div>", "div:has(.x) { color: red }");
        let paragraph = elements(&document, "p")[0];
        document.set_attribute(paragraph, "class", "x").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&elements(&document, "div")[0]));
        assert!(!affected.contains(&elements(&document, "section")[0]) && !affected.contains(&elements(&document, "div")[1]));
    }

    #[test]
    fn child_removal_restyles_the_new_first_child() {
        let (mut document, sets) = setup("<ul><li>1</li><li>2</li><li>3</li></ul>", "li:first-child { color: red }");
        let items = elements(&document, "li");
        document.remove(items[0]).unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&items[1]));
    }

    #[test]
    fn attribute_selector_reaches_the_element_and_not_its_neighbours() {
        let (mut document, sets) = setup("<p>1</p><p>2</p>", "[data-k] { color: red }");
        let second = elements(&document, "p")[1];
        document.set_attribute(second, "data-k", "v").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), Some(alloc::vec![second]));
    }

    #[test]
    fn full_rebuild_and_oversized_batches_are_not_bounded() {
        let mut html = String::from("<p class=a>0</p>");
        for n in 0..20000 {
            html.push_str(&format!("<p>{n}</p>"));
        }
        let (mut document, sets) = setup(&html, ".a ~ p { color: red }");
        let first = elements(&document, "p")[0];
        document.set_attribute(first, "class", "").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), None);
        let rebuild = Mutation { version: 1, target: document.root(), kind: MutationKind::FullRebuild };
        assert_eq!(sets.affected(&document, &[rebuild]), None);
    }

    #[test]
    fn subtree_summary_is_bounded_only_for_subtree_local_rules() {
        let html = "<div id=c><p>1</p><p>2</p></div><p>3</p>";
        let summary = |document: &Document| {
            let container = elements(document, "div")[0];
            Mutation { version: 1, target: container, kind: MutationKind::Tree { added: Some(container), removed: Some(container), styles_changed: false } }
        };
        let (document, sets) = setup(html, ".a { color: red } .a p { color: blue }");
        let affected = sets.affected(&document, &[summary(&document)]).unwrap();
        assert_eq!(affected.len(), 3);
        assert!(!affected.contains(&elements(&document, "p")[2]));
        let (document, sets) = setup(html, "p:first-child { color: red }");
        assert_eq!(sets.affected(&document, &[summary(&document)]), None);
    }

    #[test]
    fn extend_equals_building_from_all_rules() {
        let first = parse_stylesheet(".a .b { color: red } [x] { margin: 0 }").unwrap().rules;
        let second = parse_stylesheet("#i + p { color: blue } li:last-child { color: green }").unwrap().rules;
        let mut combined = first.clone();
        combined.extend(second.clone());
        let full = InvalidationSets::new(&combined);
        let mut extended = InvalidationSets::new(&first);
        extended.extend(&second);
        assert_eq!(full.classes, extended.classes);
        assert_eq!(full.ids, extended.ids);
        assert_eq!(full.attributes, extended.attributes);
        assert_eq!(full.any_class, extended.any_class);
        assert_eq!(full.class_subjects, extended.class_subjects);
        assert_eq!(full.any_class_subjects, extended.any_class_subjects);
        assert_eq!(full.structural_subjects, extended.structural_subjects);
        assert_eq!(full.relative_keys.len(), extended.relative_keys.len());
        assert_eq!(full.siblings, extended.siblings);
        assert_eq!(full.last_edge, extended.last_edge);
        assert_eq!(full.sibling_keys.len(), extended.sibling_keys.len());
    }

    fn element_with(document: &mut Document, tag: &str, attributes: &[(&str, &str)]) -> NodeId {
        let attributes = attributes.iter().map(|(name, value)| ((*name).into(), String::from(*value))).collect();
        document.create_unprefixed_element(crate::Namespace::Html, tag.into(), attributes).unwrap()
    }

    const DEEP: &str = "<div class=a id=root><section><p class=c>1</p><p>2</p><span class=c>3</span></section></div><p class=c>4</p>";

    #[test]
    fn ancestor_class_change_reaches_only_descendants_with_the_subject_features() {
        let (mut document, sets) = setup(DEEP, ".x .c { color: red }");
        let root = elements(&document, "div")[0];
        let paragraphs = elements(&document, "p");
        document.set_attribute(root, "class", "a x").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&root) && affected.contains(&paragraphs[0]));
        assert!(!affected.contains(&paragraphs[1]) && !affected.contains(&paragraphs[2]));
        assert!(affected.contains(&elements(&document, "span")[0]));
    }

    #[test]
    fn featureless_subject_reaches_the_whole_subtree() {
        let (mut document, sets) = setup(DEEP, ".x * { color: red }");
        let root = elements(&document, "div")[0];
        document.set_attribute(root, "class", "a x").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert_eq!(affected.len(), 5);
        let (mut document, sets) = setup(DEEP, ".x :not(.c) { color: red }");
        let root = elements(&document, "div")[0];
        document.set_attribute(root, "class", "a x").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()).unwrap().len(), 5);
    }

    #[test]
    fn subject_alternatives_contribute_their_features() {
        let (mut document, sets) = setup(DEEP, ".x :is(span, #nothing) { color: red }");
        let root = elements(&document, "div")[0];
        document.set_attribute(root, "class", "a x").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert_eq!(affected.len(), 2);
        assert!(affected.contains(&elements(&document, "span")[0]));
    }

    #[test]
    fn keyed_has_reaches_matching_ancestors_only() {
        let html = "<div class=box><section><p>1</p></section><div class=box id=n><i><b>t</b></i></div></div><div class=box></div>";
        let (mut document, sets) = setup(html, ".box:has(.inner) { color: red }");
        let bold = elements(&document, "b")[0];
        document.set_attribute(bold, "class", "inner").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        let boxes = elements(&document, "div");
        assert!(affected.contains(&bold) && affected.contains(&boxes[0]) && affected.contains(&boxes[1]));
        assert!(!affected.contains(&boxes[2]));
        assert!(!affected.contains(&elements(&document, "section")[0]) && !affected.contains(&elements(&document, "i")[0]));
    }

    #[test]
    fn keyed_sibling_has_reaches_preceding_siblings() {
        let (mut document, sets) = setup("<p><a>1</a><a>2</a><a>3</a></p><a>4</a>", "a:has(+ .x) { color: red }");
        let anchors = elements(&document, "a");
        document.set_attribute(anchors[2], "class", "x").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(anchors[..3].iter().all(|node| affected.contains(node)));
        assert!(!affected.contains(&elements(&document, "p")[0]));
    }

    #[test]
    fn unkeyed_has_keeps_all_relatives() {
        let (mut document, sets) = setup("<div><p>1</p><p id=t>2</p><p>3</p></div>", ":has(.inner) { color: red }");
        let target = elements(&document, "p")[1];
        document.set_attribute(target, "class", "inner").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(elements(&document, "p").iter().all(|node| affected.contains(node)));
        assert!(affected.contains(&elements(&document, "div")[0]));
    }

    #[test]
    fn form_attribute_writes_are_bounded_by_the_form() {
        let css = "input:checked { color: red } input:invalid { color: blue }";
        let (mut document, sets) = setup("<form><input id=a><input id=b></form><input id=c>", css);
        let detached = element_with(&mut document, "input", &[]);
        document.set_attribute(detached, "type", "radio").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), Some(Vec::new()));
        document.clear_mutations();
        let inputs = elements(&document, "input");
        document.set_attribute(inputs[0], "type", "checkbox").unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&inputs[0]) && affected.contains(&inputs[1]) && !affected.contains(&inputs[2]));
        document.clear_mutations();
        document.set_attribute(inputs[2], "type", "radio").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), None);
        document.clear_mutations();
        document.set_attribute(inputs[2], "form", "f").unwrap();
        assert_eq!(sets.affected(&document, document.mutations()), None);
    }

    #[test]
    fn child_list_change_reaches_only_structural_subjects_below() {
        let (mut document, sets) = setup("<div id=w><section><p class=x>a</p><p>b</p><i>c</i></section></div>", ":first-child .x { color: red }");
        let wrapper = elements(&document, "div")[0];
        let added = element_with(&mut document, "span", &[]);
        document.insert_before(wrapper, added, None).unwrap();
        let affected = sets.affected(&document, document.mutations()).unwrap();
        assert!(affected.contains(&elements(&document, "p")[0]));
        assert!(!affected.contains(&elements(&document, "p")[1]) && !affected.contains(&elements(&document, "i")[0]));
    }

}
