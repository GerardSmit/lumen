//! HTML details transitions, independent of an event loop or language host.
use alloc::rc::Rc;
use crate::{Document, Error, Namespace, NodeId, NodeKind, selector, svg};

/// A real change of the null-namespace `open` attribute presence. Hosts feed
/// these notifications into their existing HTML toggle-task coalescer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DetailsTransition {
    pub node: NodeId,
    pub old_open: bool,
    pub new_open: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::String, vec, vec::Vec};
    use core::cell::RefCell;
    use crate::Name;

    fn element(doc: &mut Document, attrs: &[(&str, &str)]) -> NodeId {
        doc.create(NodeKind::Element {
            namespace: Namespace::Html, name: Name::new("details"),
            attributes: attrs.iter().map(|(key,value)| (Name::new(key),String::from(*value))).collect(),
        }).unwrap()
    }

    fn observed() -> (Document, Rc<RefCell<Vec<DetailsTransition>>>) {
        let mut doc = Document::new(1024);
        let events = Rc::new(RefCell::new(Vec::new()));
        let capture = events.clone();
        doc.set_details_transition_sink(Some(Rc::new(move |_, event| capture.borrow_mut().push(event))));
        (doc, events)
    }

    #[test]
    fn details_summary_selection_and_conditional_interactive_membership_are_shared() {
        let (mut doc, events) = observed();
        let fragment = crate::html::parse_fragment(&mut doc, "<details><p>before</p><summary id=first><span id=plain>text</span><a id=anchor>anchor</a><input id=hidden type=HIDDEN><audio id=audio></audio><img id=image></summary><summary id=second>second</summary></details>").unwrap();
        let details = doc.first_child(fragment).unwrap().unwrap();
        let node = |doc: &Document, selector: &str| crate::selector::query_selector(doc, fragment, selector).unwrap().unwrap();
        let plain = node(&doc,"#plain");
        assert_eq!(doc.summary_activation_details(plain),Some(details));
        assert_eq!(doc.summary_activation_details(node(&doc,"#second")),None);
        for (id, attribute) in [("#anchor","href"),("#audio","controls"),("#image","usemap")] {
            let target = node(&doc,id);
            assert_eq!(doc.summary_activation_details(target),Some(details));
            doc.set_attribute_ns(target,Some("urn:test"),attribute,"").unwrap();
            assert_eq!(doc.summary_activation_details(target),Some(details));
            doc.set_attribute_ns(target,None,attribute,"").unwrap();
            assert_eq!(doc.summary_activation_details(target),None);
        }
        let hidden = node(&doc,"#hidden");
        assert_eq!(doc.summary_activation_details(hidden),Some(details));
        doc.set_attribute(hidden,"type","text").unwrap();
        assert_eq!(doc.summary_activation_details(hidden),None);
        assert!(doc.activate_summary(plain).unwrap());
        assert_eq!(doc.details_open_state(details),Some(true));
        assert!(doc.activate_summary(plain).unwrap());
        assert_eq!(doc.details_open_state(details),Some(false));
        assert_eq!(events.borrow().len(),2);
        let first = node(&doc,"#first");
        let second = node(&doc,"#second");
        doc.insert_before(details,second,Some(first)).unwrap();
        assert_eq!(doc.summary_activation_details(plain),None);
        assert_eq!(doc.summary_activation_details(second),Some(details));
    }

    #[test]
    fn details_summary_requires_direct_html_child_and_html_details_parent() {
        let mut doc = Document::new(64);
        let fragment = crate::html::parse_fragment(&mut doc,
            "<details><div><summary id=nested>nested</summary></div><summary id=direct>direct</summary></details>").unwrap();
        let details = doc.first_child(fragment).unwrap().unwrap();
        let nested = crate::selector::query_selector(&doc,fragment,"#nested").unwrap().unwrap();
        let direct = crate::selector::query_selector(&doc,fragment,"#direct").unwrap().unwrap();
        assert_eq!(doc.summary_activation_details(nested),None);
        let foreign = doc.create(NodeKind::Element { namespace:Namespace::Svg,
            name:Name::new("summary"),attributes:Vec::new() }).unwrap();
        doc.insert_before(details,foreign,Some(direct)).unwrap();
        assert_eq!(doc.summary_activation_details(foreign),None);
        assert_eq!(doc.summary_activation_details(direct),Some(details));
        let foreign_details = doc.create(NodeKind::Element { namespace:Namespace::Svg,
            name:Name::new("details"),attributes:Vec::new() }).unwrap();
        doc.append(foreign_details,direct).unwrap();
        assert_eq!(doc.summary_activation_details(direct),None);
    }

    #[test]
    fn details_null_namespace_presence_and_attr_routes_emit_real_transitions() {
        let (mut doc, events) = observed();
        let node = element(&mut doc, &[]);
        doc.set_attribute_ns(node, Some("urn:test"), "open", "").unwrap();
        assert!(events.borrow().is_empty());
        doc.set_attribute_ns(node, None, "open", "first").unwrap();
        let attr = doc.attribute_node_by_ns(node, None, "open").unwrap().unwrap();
        doc.set_attribute_node_value(attr, "second").unwrap();
        assert_eq!(events.borrow().len(), 1);
        doc.remove_attribute_node(node, attr).unwrap();
        doc.set_attribute_node(node, attr, true).unwrap();
        doc.remove_attribute_ns(node, None, "open").unwrap();
        assert_eq!(*events.borrow(), vec![
            DetailsTransition { node, old_open:false, new_open:true },
            DetailsTransition { node, old_open:true, new_open:false },
            DetailsTransition { node, old_open:false, new_open:true },
            DetailsTransition { node, old_open:true, new_open:false },
        ]);
    }

    #[test]
    fn details_groups_distinguish_opening_naming_and_inserting() {
        let (mut doc, events) = observed();
        let first = element(&mut doc, &[("name", "g"),("open", "")]);
        let second = element(&mut doc, &[("name", "g")]);
        let root = doc.create(NodeKind::DocumentFragment).unwrap();
        doc.append(root, first).unwrap();
        doc.append(root, second).unwrap();
        events.borrow_mut().clear();
        doc.set_attribute(second, "open", "").unwrap();
        assert_eq!(doc.details_open_state(first), Some(false));
        assert_eq!(doc.details_open_state(second), Some(true));
        assert_eq!(events.borrow().iter().map(|e| (e.node,e.new_open)).collect::<Vec<_>>(), vec![(second,true),(first,false)]);
        let third = element(&mut doc, &[("name", "g"),("open", "")]);
        doc.append(root, third).unwrap();
        assert_eq!(doc.details_open_state(third), Some(false));
        let fourth = element(&mut doc, &[("open", "")]);
        doc.append(root, fourth).unwrap();
        let name = doc.create_attribute(None,"name","g").unwrap();
        doc.set_attribute_node(fourth,name,false).unwrap();
        assert_eq!(doc.details_open_state(fourth), Some(false));
        let fifth = element(&mut doc, &[("open", ""),("name", "other")]);
        doc.append(root,fifth).unwrap();
        let name = doc.attribute_node_by_ns(fifth,None,"name").unwrap().unwrap();
        doc.set_attribute_node_value(name,"g").unwrap();
        assert_eq!(doc.details_open_state(fifth), Some(false));
    }

    #[test]
    fn details_groups_follow_ordinary_tree_and_replacement_routes() {
        let (mut doc, events) = observed();
        let first = element(&mut doc, &[("name", "g"),("open", "")]);
        let second = element(&mut doc, &[("name", "g"),("open", "")]);
        assert_eq!(doc.details_open_state(second),Some(true)); // separate detached roots
        let parent = doc.create(NodeKind::DocumentFragment).unwrap();
        doc.replace_children_many(parent,&[first,second]).unwrap();
        assert_eq!(doc.details_open_state(first),Some(true));
        assert_eq!(doc.details_open_state(second),Some(false));
        doc.set_attribute(first,"name", "").unwrap();
        doc.set_attribute(second,"name", "").unwrap();
        doc.set_attribute(second,"open", "").unwrap();
        assert_eq!(doc.details_open_state(first),Some(true));
        assert_eq!(doc.details_open_state(second),Some(true));
        doc.set_details_transition_sink(None);
        let count = events.borrow().len();
        doc.remove_attribute(first,"open").unwrap();
        assert_eq!(events.borrow().len(),count);
    }

    #[test]
    fn details_parser_insertion_enforces_groups_and_keeps_observer_sink() {
        let (mut doc, events) = observed();
        let mutations = Rc::new(RefCell::new(0));
        let capture = mutations.clone();
        doc.set_mutation_sink(Some(Rc::new(move |_, _| *capture.borrow_mut() += 1)));
        let fragment = crate::html::parse_fragment(&mut doc,
            "<details name=g open></details><details name=g open></details>").unwrap();
        let first = doc.first_child(fragment).unwrap().unwrap();
        let second = doc.next_sibling(first).unwrap().unwrap();
        assert_eq!(doc.details_open_state(first), Some(true));
        assert_eq!(doc.details_open_state(second), Some(false));
        assert_eq!(*events.borrow(), vec![
            DetailsTransition { node:first, old_open:false, new_open:true },
            DetailsTransition { node:second, old_open:false, new_open:true },
            DetailsTransition { node:second, old_open:true, new_open:false },
        ]);
        assert!(*mutations.borrow() > 0);
    }

    #[test]
    fn details_namespaced_attr_replacement_and_clone_preserve_real_presence() {
        let (mut doc, events) = observed();
        let node = element(&mut doc, &[("open", "")]);
        events.borrow_mut().clear();
        let namespaced = doc.create_attribute(Some("urn:test"), "open", "").unwrap();
        doc.set_attribute_node(node, namespaced, false).unwrap();
        assert_eq!(doc.details_open_state(node),Some(false));
        assert_eq!(*events.borrow(), vec![DetailsTransition { node, old_open:true, new_open:false }]);
        events.borrow_mut().clear();
        let clone = doc.clone_shallow(node).unwrap();
        assert_eq!(doc.details_open_state(clone),Some(false));
        assert!(events.borrow().is_empty());
        // The ordinary null-namespace attribute may coexist with the same QName.
        doc.set_attribute_ns(clone,None,"open", "").unwrap();
        assert_eq!(doc.details_open_state(clone),Some(true));
        assert_eq!(events.borrow().len(),1);
    }

    #[test]
    fn details_shadow_trees_have_independent_group_scope() {
        let (mut doc, _) = observed();
        let host = doc.create(NodeKind::Element {
            namespace:Namespace::Html,name:Name::new("div"),attributes:Vec::new()
        }).unwrap();
        let light = element(&mut doc,&[("name","g"),("open", "")]);
        doc.append(host,light).unwrap();
        let shadow = doc.attach_shadow(host,crate::ShadowMode::Open).unwrap();
        let inner = element(&mut doc,&[("name","g"),("open", "")]);
        doc.append(shadow,inner).unwrap();
        assert_eq!(doc.details_open_state(light),Some(true));
        assert_eq!(doc.details_open_state(inner),Some(true));
        doc.append(host,inner).unwrap();
        assert_eq!(doc.details_open_state(light),Some(true));
        assert_eq!(doc.details_open_state(inner),Some(false));
    }
}

impl Document {
    /// Resolve the nearest summary activation target in the ordinary tree.
    /// Interactive descendants own their interaction; only the first HTML
    /// summary child of an HTML details may activate that details element.
    pub fn summary_activation_details(&self, target: NodeId) -> Option<NodeId> {
        let mut current = Some(target);
        while let Some(node) = current {
            if let Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) = self.kind(node) {
                if svg::local_name(name) == "summary" {
                    let parent = self.parent(node).ok()??;
                    self.details_open_state(parent)?;
                    let mut child = self.first_child(parent).ok()?;
                    while let Some(candidate) = child {
                        if matches!(self.kind(candidate).ok(), Some(NodeKind::Element {
                            namespace: Namespace::Html, name, ..
                        }) if svg::local_name(name) == "summary") {
                            return (candidate == node).then_some(parent);
                        }
                        child = self.next_sibling(candidate).ok()?;
                    }
                    return None;
                }
                if self.is_interactive_content(node) { return None; }
            }
            current = self.parent(node).ok()?;
        }
        None
    }

    /// HTML interactive-content membership depends on null-namespace
    /// attributes, including the Hidden input type state.
    pub fn is_interactive_content(&self, node: NodeId) -> bool {
        let Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) = self.kind(node) else { return false; };
        let present = |attribute| self.get_attribute_ns_ref(node, None, attribute)
            .ok().flatten().is_some();
        match svg::local_name(name) {
            "button" | "details" | "embed" | "iframe" | "label" | "select" | "textarea" => true,
            "a" => present("href"),
            "audio" | "video" => present("controls"),
            "img" => present("usemap") || present("controls"),
            "input" => !self.get_attribute_ns_ref(node, None, "type").ok().flatten()
                .is_some_and(|kind| kind.eq_ignore_ascii_case("hidden")),
            _ => false,
        }
    }

    /// Apply real summary activation through the sole central open-attribute
    /// transition path. Hosts select again after cancellable click listeners.
    pub fn activate_summary(&mut self, target: NodeId) -> Result<bool, Error> {
        let Some(details) = self.summary_activation_details(target) else { return Ok(false); };
        if self.details_open_state(details) == Some(true) {
            self.remove_attribute_ns(details, None, "open")?;
        } else {
            self.set_attribute_ns(details, None, "open", "")?;
        }
        Ok(true)
    }

    /// Install before parsing or exposing this document to author mutations.
    /// The observer must only record native work: it must not execute author
    /// code or mutate this borrowed document. No notifications are buffered
    /// when no observer is installed. Task admission is a host responsibility.
    pub fn set_details_transition_sink(
        &mut self,
        sink: Option<Rc<dyn Fn(&Document, DetailsTransition)>>,
    ) {
        self.details_sink = sink;
    }

    pub(crate) fn details_open_state(&self, id: NodeId) -> Option<bool> {
        match self.kind(id).ok()? {
            NodeKind::Element { namespace: Namespace::Html, name, .. }
                if svg::local_name(name) == "details" =>
                    Some(self.get_attribute_ns_ref(id, None, "open").ok()?.is_some()),
            _ => None,
        }
    }

    fn details_notify(&self, id: NodeId, old_open: bool, new_open: bool) {
        if let Some(sink) = &self.details_sink {
            sink(self, DetailsTransition { node: id, old_open, new_open });
        }
    }

    pub(crate) fn details_created(&mut self, id: NodeId) -> Result<(), Error> {
        if self.details_open_state(id) == Some(true) {
            self.details_notify(id, false, true);
        }
        Ok(())
    }

    pub(crate) fn details_attribute_changed(
        &mut self,
        id: NodeId,
        old_open: Option<bool>,
        name_changed: bool,
    ) -> Result<(), Error> {
        let Some(old_open) = old_open else { return Ok(()); };
        let new_open = self.details_open_state(id).ok_or(Error::WrongKind)?;
        if old_open != new_open {
            self.details_notify(id, old_open, new_open);
        }
        if new_open && !old_open {
            // Opening closes the first other open member, not this member.
            if let Some(other) = self.details_open_peer(id)? {
                self.remove_attribute_ns(other, None, "open")?;
            }
        } else if new_open && name_changed {
            self.details_close_if_group_open(id)?;
        }
        Ok(())
    }

    fn details_open_peer(&self, id: NodeId) -> Result<Option<NodeId>, Error> {
        let Some(name) = self.get_attribute_ns_ref(id, None, "name")?
            .filter(|name| !name.is_empty()) else { return Ok(None); };
        let mut root = id;
        while let Some(parent) = self.parent(root)? { root = parent; }
        let mut current = Some(root);
        while let Some(candidate) = current {
            if candidate != id && self.details_open_state(candidate) == Some(true)
                && self.get_attribute_ns_ref(candidate, None, "name")? == Some(name)
            {
                return Ok(Some(candidate));
            }
            current = selector::next_descendant(self, root, candidate)?;
        }
        Ok(None)
    }

    fn details_close_if_group_open(&mut self, id: NodeId) -> Result<(), Error> {
        if self.details_open_state(id) == Some(true) && self.details_open_peer(id)?.is_some() {
            self.remove_attribute_ns(id, None, "open")?;
        }
        Ok(())
    }

    pub(crate) fn details_inserted(&mut self, root: NodeId) -> Result<(), Error> {
        // Shadow trees and template contents have separate roots/group scopes.
        let mut current = Some(root);
        while let Some(id) = current {
            self.details_close_if_group_open(id)?;
            current = selector::next_descendant(self, root, id)?;
        }
        Ok(())
    }
}
