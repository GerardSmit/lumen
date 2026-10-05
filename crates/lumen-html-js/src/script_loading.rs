//! Native script preparation state for the active DOM realm.
//!
//! Fetching external scripts and evaluating modules belongs to the embedder's
//! document loader. This adapter executes classic inline scripts and captures
//! module preparations for an explicitly enabled host loader. Other resource-
//! backed activations remain explicit unsupported diagnostics.

use lumen_html::{
    Document, Namespace, NodeId, NodeKind,
    observe::{ObservedKind, ObservedMutation},
};
use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnhandledScriptReason {
    ExternalSource,
    Module,
    ImportMap,
    SpeculationRules,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnhandledScriptActivation {
    pub node: NodeId,
    pub reason: UnhandledScriptReason,
    pub src: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct ScriptState {
    already_started: bool,
    parser_inserted: bool,
    force_async: bool,
}

impl Default for ScriptState {
    fn default() -> Self {
        Self {
            already_started: false,
            parser_inserted: false,
            force_async: true,
        }
    }
}

#[derive(Default)]
pub(crate) struct ScriptLoader {
    states: HashMap<NodeId, ScriptState>,
    queued: VecDeque<NodeId>,
    queued_set: HashSet<NodeId>,
    unhandled: Vec<UnhandledScriptActivation>,
    unhandled_nodes: HashSet<NodeId>,
}

impl ScriptLoader {
    pub(crate) fn register_parser_script(&mut self, node: NodeId) {
        let state = self.states.entry(node).or_default();
        state.parser_inserted = true;
        state.force_async = false;
    }

    pub(crate) fn force_async(&self, node: NodeId) -> bool {
        self.states
            .get(&node)
            .map_or(true, |state| state.force_async)
    }

    pub(crate) fn clear_force_async(&mut self, node: NodeId) {
        self.states.entry(node).or_default().force_async = false;
    }

    pub(crate) fn mark_started(&mut self, node: NodeId) {
        let state = self.states.entry(node).or_default();
        state.already_started = true;
        state.parser_inserted = false;
        self.queued_set.remove(&node);
        self.queued.retain(|candidate| *candidate != node);
    }

    pub(crate) fn mark_parser_prepared(&mut self, node: NodeId) {
        self.states.entry(node).or_default().parser_inserted = false;
    }

    pub(crate) fn already_started(&self, node: NodeId) -> bool {
        self.states
            .get(&node)
            .is_some_and(|state| state.already_started)
    }

    pub(crate) fn parser_inserted(&self, node: NodeId) -> bool {
        self.states
            .get(&node)
            .is_some_and(|state| state.parser_inserted)
    }

    pub(crate) fn queue(&mut self, node: NodeId) {
        if self.queued_set.insert(node) {
            self.queued.push_back(node);
        }
    }

    pub(crate) fn take_next(&mut self) -> Option<NodeId> {
        let node = self.queued.pop_front()?;
        self.queued_set.remove(&node);
        Some(node)
    }

    pub(crate) fn record_unhandled(
        &mut self,
        node: NodeId,
        reason: UnhandledScriptReason,
        src: Option<String>,
    ) {
        if self.unhandled_nodes.insert(node) {
            self.unhandled
                .push(UnhandledScriptActivation { node, reason, src });
        }
    }

    pub(crate) fn unhandled(&self) -> Vec<UnhandledScriptActivation> {
        self.unhandled.clone()
    }

    pub(crate) fn clone_state(&mut self, original: NodeId, copy: NodeId) {
        let already_started = self.already_started(original);
        if already_started {
            self.states.insert(
                copy,
                ScriptState {
                    already_started: true,
                    parser_inserted: false,
                    force_async: true,
                },
            );
        } else {
            self.states.remove(&copy);
        }
    }

    pub(crate) fn clone_states(&mut self, pairs: &[(NodeId, NodeId)]) {
        for &(original, copy) in pairs {
            self.clone_state(original, copy);
        }
    }

    pub(crate) fn clone_states_from(&mut self, source: &Self, pairs: &[(NodeId, NodeId)]) {
        for &(original, copy) in pairs {
            if source.already_started(original) {
                self.states.insert(
                    copy,
                    ScriptState {
                        already_started: true,
                        parser_inserted: false,
                        force_async: true,
                    },
                );
            } else {
                self.states.remove(&copy);
            }
        }
    }

    pub(crate) fn adopt_nodes_into(&mut self, target: &mut Self, mapping: &[(NodeId, NodeId)]) {
        for &(old, new) in mapping {
            if let Some(state) = self.states.remove(&old) {
                target.states.insert(new, state);
            }
            if self.queued_set.remove(&old) {
                self.queued.retain(|node| *node != old);
                if target.queued_set.insert(new) {
                    target.queued.push_back(new);
                }
            }
            if self.unhandled_nodes.remove(&old) {
                target.unhandled_nodes.insert(new);
                if let Some(activation) = self.unhandled.iter_mut().find(|entry| entry.node == old)
                {
                    let mut activation = activation.clone();
                    activation.node = new;
                    target.unhandled.push(activation);
                }
                self.unhandled.retain(|activation| activation.node != old);
            }
        }
    }

    pub(crate) fn on_mutation(
        &mut self,
        document: &Document,
        mutation: &ObservedMutation,
        is_html_document: bool,
    ) {
        if let ObservedKind::Attribute {
            name,
            namespace_uri,
            ..
        } = &mutation.kind
        {
            let is_async = if is_html_document {
                name.eq_ignore_ascii_case("async")
            } else {
                name == "async"
            };
            if is_async && namespace_uri.is_none() && is_script(document, mutation.target) {
                if let Ok(NodeKind::Element { attributes, .. }) = document.kind(mutation.target) {
                    if attributes.iter().enumerate().any(|(index, (key, _))| {
                        key == "async"
                            && document
                                .attribute_namespace_uri_at(mutation.target, index)
                                .is_none()
                    }) {
                        self.clear_force_async(mutation.target);
                    }
                }
            }
        }
        match &mutation.kind {
            ObservedKind::ChildList { added, .. } => {
                if added.is_some()
                    && is_script(document, mutation.target)
                    && is_connected(document, mutation.target)
                {
                    self.queue(mutation.target);
                }
                if let Some(added) = added {
                    self.queue_connected_scripts(document, *added);
                }
            }
            ObservedKind::ChildListMany { added, .. } => {
                if !added.is_empty()
                    && is_script(document, mutation.target)
                    && is_connected(document, mutation.target)
                {
                    self.queue(mutation.target);
                }
                for node in added {
                    self.queue_connected_scripts(document, *node);
                }
            }
            ObservedKind::Attribute {
                name,
                namespace_uri,
                ..
            } if source_mutation(
                document,
                mutation.target,
                name,
                namespace_uri.as_deref(),
                is_html_document,
            ) && is_script(document, mutation.target)
                && is_connected(document, mutation.target) =>
            {
                self.queue(mutation.target);
            }
            _ => {}
        }
    }

    fn queue_connected_scripts(&mut self, document: &Document, root: NodeId) {
        for node in descendant_nodes(document, root) {
            if is_script(document, node) && is_connected(document, node) {
                self.queue(node);
            }
        }
    }
}

pub(crate) enum ScriptKind {
    ClassicInline(String),
    SuppressedClassic,
    InvalidSource,
    Unhandled(UnhandledScriptReason, Option<String>),
    DataBlock,
}

#[cfg(test)]
mod preparation_tests {
    use super::*;

    #[test]
    fn source_preparation_rejects_only_empty_html_whitespace_classic_and_module_urls() {
        let mut document = Document::new(64);
        let script = crate::html_element(&mut document, "script").unwrap();
        for kind in ["", "module"] {
            document.set_attribute(script, "type", kind).unwrap();
            for source in ["", " ", "\t\n\r\u{000c} "] {
                document.set_attribute(script, "src", source).unwrap();
                assert!(matches!(prepare_kind(&document, script, true), Some(ScriptKind::InvalidSource)));
                assert_eq!(source_attribute(&document, script, true).as_deref(), Some(source));
            }
            for source in ["\u{00a0}", "\u{000b}", " \tvalid.js\r\n"] {
                document.set_attribute(script, "src", source).unwrap();
                assert!(matches!(prepare_kind(&document, script, true), Some(ScriptKind::Unhandled(_, _))));
            }
        }
        document.set_attribute(script, "src", "   ").unwrap();
        document.set_attribute(script, "type", "application/json").unwrap();
        assert!(matches!(prepare_kind(&document, script, true), Some(ScriptKind::DataBlock)));
        document.set_attribute(script, "type", "").unwrap();
        document.set_attribute(script, "nomodule", "").unwrap();
        assert!(matches!(prepare_kind(&document, script, true), Some(ScriptKind::SuppressedClassic)));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredScriptType {
    Classic,
    SuppressedClassic,
    Module,
    ImportMap,
    SpeculationRules,
    DataBlock,
}

/// Native snapshot used by the embedder's document loader. Script identity and
/// classification come from the actual namespace-aware DOM, without invoking
/// author-modified query, Array or attribute functions.
#[derive(Clone, Debug)]
pub struct ScriptDescriptor {
    pub node: NodeId,
    pub namespace: Namespace,
    pub kind: DeclaredScriptType,
    pub src: Option<String>,
    pub text: String,
    pub is_async: bool,
    pub defer: bool,
    pub parser_inserted: bool,
}

pub(crate) fn is_script(document: &Document, node: NodeId) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element { namespace: Namespace::Html | Namespace::Svg, name, .. })
            if lumen_html::xml::split_qname(name.as_str()).is_some_and(|(_, local)| local == "script")
    )
}

pub(crate) fn is_connected(document: &Document, node: NodeId) -> bool {
    let root = document.root();
    let mut current = Some(node);
    while let Some(id) = current {
        if id == root {
            return true;
        }
        current = document.shadow_including_parent(id).ok().flatten();
    }
    false
}

pub(crate) fn descendant_nodes(document: &Document, root: NodeId) -> Vec<NodeId> {
    let mut result = Vec::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        result.push(node);
        let mut children = Vec::new();
        let mut child = document.first_child(node).ok().flatten();
        while let Some(id) = child {
            children.push(id);
            child = document.next_sibling(id).ok().flatten();
        }
        pending.extend(children.into_iter().rev());
    }
    result
}

pub(crate) fn parser_scripts(document: &Document) -> Vec<NodeId> {
    descendant_nodes(document, document.root())
        .into_iter()
        .filter(|node| is_script(document, *node))
        .collect()
}

/// Script elements in template contents are not parser-blocking scripts in the
/// document tree. They do, however, carry the parser's inert/already-started
/// state when their template contents are later moved into a live tree.
pub(crate) fn template_scripts(document: &Document) -> Vec<NodeId> {
    let mut result = Vec::new();
    let mut pending = vec![(document.root(), false)];
    let mut visited = HashSet::new();
    while let Some((node, in_template_contents)) = pending.pop() {
        if !visited.insert(node) {
            continue;
        }
        if in_template_contents && is_script(document, node) {
            result.push(node);
        }
        if let Ok(Some(content)) = document.template_content(node) {
            pending.push((content, true));
        }
        let mut child = document.first_child(node).ok().flatten();
        while let Some(id) = child {
            pending.push((id, in_template_contents));
            child = document.next_sibling(id).ok().flatten();
        }
    }
    result
}

pub(crate) fn paired_subtree_nodes(
    original_document: &Document,
    copy_document: &Document,
    original_root: NodeId,
    copy_root: NodeId,
) -> Vec<(NodeId, NodeId)> {
    let mut pairs = Vec::new();
    let mut pending = vec![(original_root, copy_root)];
    while let Some((original, copy)) = pending.pop() {
        pairs.push((original, copy));
        if let (Ok(Some(original_content)), Ok(Some(copy_content))) = (
            original_document.template_content(original),
            copy_document.template_content(copy),
        ) {
            pending.push((original_content, copy_content));
        }
        if let (Ok(Some(original_shadow)), Ok(Some(copy_shadow))) = (
            original_document.shadow_root(original),
            copy_document.shadow_root(copy),
        ) {
            pending.push((original_shadow, copy_shadow));
        }
        let mut original_child = original_document.first_child(original).ok().flatten();
        let mut copy_child = copy_document.first_child(copy).ok().flatten();
        while let (Some(original_node), Some(copy_node)) = (original_child, copy_child) {
            pending.push((original_node, copy_node));
            original_child = original_document.next_sibling(original_node).ok().flatten();
            copy_child = copy_document.next_sibling(copy_node).ok().flatten();
        }
    }
    pairs
}

pub(crate) fn scripts_in_subtree(document: &Document, root: NodeId) -> Vec<NodeId> {
    descendant_nodes(document, root)
        .into_iter()
        .filter(|node| is_script(document, *node))
        .collect()
}

pub(crate) fn prepare_kind(
    document: &Document,
    node: NodeId,
    is_html_document: bool,
) -> Option<ScriptKind> {
    if !is_script(document, node) {
        return None;
    }
    let source = source_attribute(document, node, is_html_document);
    let text = script_child_text(document, node);
    // An empty connected element is still eligible when text is added later.
    if source.is_none() && text.is_empty() {
        return None;
    }
    match type_from_document(document, node, is_html_document) {
        DeclaredScriptType::SuppressedClassic => return Some(ScriptKind::SuppressedClassic),
        DeclaredScriptType::Module => {
            if source.as_deref().is_some_and(lumen_common::scan::is_ascii_whitespace_only) {
                return Some(ScriptKind::InvalidSource);
            }
            return Some(ScriptKind::Unhandled(UnhandledScriptReason::Module, source));
        }
        DeclaredScriptType::ImportMap => {
            return Some(ScriptKind::Unhandled(
                UnhandledScriptReason::ImportMap,
                source,
            ));
        }
        DeclaredScriptType::SpeculationRules => {
            return Some(ScriptKind::Unhandled(
                UnhandledScriptReason::SpeculationRules,
                source,
            ));
        }
        DeclaredScriptType::DataBlock => return Some(ScriptKind::DataBlock),
        DeclaredScriptType::Classic => {}
    }
    if source.is_some() {
        if source.as_deref().is_some_and(lumen_common::scan::is_ascii_whitespace_only) {
            return Some(ScriptKind::InvalidSource);
        }
        return Some(ScriptKind::Unhandled(
            UnhandledScriptReason::ExternalSource,
            source,
        ));
    }
    Some(ScriptKind::ClassicInline(text))
}

pub(crate) fn script_child_text(document: &Document, node: NodeId) -> String {
    let mut text = String::new();
    let mut child = document.first_child(node).ok().flatten();
    while let Some(id) = child {
        if let Ok(NodeKind::Text(value) | NodeKind::CData(value)) = document.kind(id) {
            text.push_str(value);
        }
        child = document.next_sibling(id).ok().flatten();
    }
    text
}

fn type_from_attributes(attribute: impl Fn(&str) -> Option<String>) -> DeclaredScriptType {
    let kind = match attribute("type") {
        Some(value) if value.is_empty() => "text/javascript".to_owned(),
        Some(value) => value
            .trim_matches(['\t', '\n', '\u{000C}', '\r', ' '])
            .to_owned(),
        None => match attribute("language") {
            Some(value) if !value.is_empty() => format!("text/{value}"),
            _ => "text/javascript".to_owned(),
        },
    };
    declared_script_type(&kind.to_ascii_lowercase(), attribute("nomodule").is_some())
}

pub(crate) fn declared_script_type(essence: &str, has_nomodule: bool) -> DeclaredScriptType {
    let kind = match essence {
        "module" => DeclaredScriptType::Module,
        "importmap" => DeclaredScriptType::ImportMap,
        "speculationrules" => DeclaredScriptType::SpeculationRules,
        "application/ecmascript"
        | "application/javascript"
        | "application/x-ecmascript"
        | "application/x-javascript"
        | "text/ecmascript"
        | "text/javascript"
        | "text/javascript1.0"
        | "text/javascript1.1"
        | "text/javascript1.2"
        | "text/javascript1.3"
        | "text/javascript1.4"
        | "text/javascript1.5"
        | "text/jscript"
        | "text/livescript"
        | "text/x-ecmascript"
        | "text/x-javascript" => DeclaredScriptType::Classic,
        _ => DeclaredScriptType::DataBlock,
    };
    if has_nomodule && kind == DeclaredScriptType::Classic {
        DeclaredScriptType::SuppressedClassic
    } else {
        kind
    }
}

pub(crate) fn is_classic_type(document: &Document, node: NodeId, is_html_document: bool) -> bool {
    is_script(document, node)
        && type_from_document(document, node, is_html_document) == DeclaredScriptType::Classic
}

fn attribute(
    document: &Document,
    node: NodeId,
    wanted: &str,
    is_html_document: bool,
) -> Option<String> {
    let NodeKind::Element {
        namespace,
        attributes,
        ..
    } = document.kind(node).ok()?
    else {
        return None;
    };
    let fold_case = is_html_document && *namespace == Namespace::Html;
    attributes
        .iter()
        .enumerate()
        .find(|(index, (name, _))| {
            if fold_case {
                document.attribute_namespace_uri_at(node, *index).is_none()
                    && name.as_str().eq_ignore_ascii_case(wanted)
            } else {
                document.attribute_namespace_uri_at(node, *index).is_none()
                    && name.as_str() == wanted
            }
        })
        .map(|(_, (_, value))| value.clone())
}

fn is_svg(document: &Document, node: NodeId) -> bool {
    matches!(
        document.kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Svg,
            ..
        })
    )
}

fn source_attribute(document: &Document, node: NodeId, is_html_document: bool) -> Option<String> {
    if is_svg(document, node) {
        document
            .get_attribute_ns(node, None, "href")
            .ok()
            .flatten()
            .or_else(|| {
                document
                    .get_attribute_ns(node, Some("http://www.w3.org/1999/xlink"), "href")
                    .ok()
                    .flatten()
            })
    } else {
        attribute(document, node, "src", is_html_document)
    }
}

fn source_mutation(
    document: &Document,
    node: NodeId,
    name: &str,
    namespace_uri: Option<&str>,
    is_html_document: bool,
) -> bool {
    if is_svg(document, node) {
        (name == "href" && namespace_uri.is_none())
            || (lumen_html::xml::split_qname(name).is_some_and(|(_, local)| local == "href")
                && namespace_uri == Some("http://www.w3.org/1999/xlink"))
    } else if is_html_document {
        namespace_uri.is_none() && name.eq_ignore_ascii_case("src")
    } else {
        namespace_uri.is_none() && name == "src"
    }
}

fn type_from_document(
    document: &Document,
    node: NodeId,
    is_html_document: bool,
) -> DeclaredScriptType {
    let svg = is_svg(document, node);
    type_from_attributes(|name| {
        if svg && matches!(name, "language" | "nomodule") {
            None
        } else {
            attribute(document, node, name, is_html_document)
        }
    })
}

pub(crate) fn descriptor(
    document: &Document,
    node: NodeId,
    is_html_document: bool,
    force_async: bool,
    parser_inserted: bool,
) -> Option<ScriptDescriptor> {
    is_script(document, node).then(|| ScriptDescriptor {
        node,
        namespace: match document.kind(node).expect("script element exists") {
            NodeKind::Element { namespace, .. } => namespace.clone(),
            _ => unreachable!("script descriptor requires an element"),
        },
        kind: type_from_document(document, node, is_html_document),
        src: source_attribute(document, node, is_html_document),
        text: script_child_text(document, node),
        is_async: force_async || attribute(document, node, "async", is_html_document).is_some(),
        defer: attribute(document, node, "defer", is_html_document).is_some(),
        parser_inserted,
    })
}
