use super::*;
use lumen_bind::This;

#[derive(Clone)]
enum Nodes {
    Children(NodeId, bool),
    Descendants(NodeId, DescendantFilter),
    SelectOptions(NodeId),
    DatalistOptions(NodeId),
    SelectedOptions(NodeId),
    FormElements(NodeId),
    FieldsetElements(NodeId),
    FormElementNames(NodeId, String),
    LabelAssociations(NodeId),
    Static(Vec<NodeId>),
}

#[derive(Clone)]
pub(crate) enum DescendantFilter {
    Tag(String),
    TagNs(Option<String>, String),
    Class(Vec<String>),
    Name(String),
    WindowNamed(String),
    FormNamedImages(NodeId, String),
}

impl DescendantFilter {
    pub(crate) fn matches(
        &self,
        document: &lumen_html::Document,
        id: NodeId,
    ) -> Result<bool, Error> {
        let NodeKind::Element {
            namespace, name, ..
        } = document.kind(id)?
        else {
            return Ok(false);
        };
        Ok(match self {
            Self::Tag(wanted) if wanted == "*" => true,
            Self::Tag(wanted) => match namespace {
                Namespace::Html => name.eq_ignore_ascii_case(wanted),
                _ => name.as_str() == wanted,
            },
            Self::TagNs(wanted_namespace, wanted_name) => {
                let uri = match namespace {
                    Namespace::Html => Some("http://www.w3.org/1999/xhtml"),
                    Namespace::Svg => Some("http://www.w3.org/2000/svg"),
                    Namespace::MathMl => Some("http://www.w3.org/1998/Math/MathML"),
                    Namespace::Other(value) if value.is_empty() => None,
                    Namespace::Other(value) => Some(value.as_ref()),
                };
                (wanted_namespace.as_deref() == Some("*") || wanted_namespace.as_deref() == uri)
                    && (wanted_name == "*" || name.as_str() == wanted_name)
            }
            Self::Class(wanted) => {
                if wanted.is_empty() {
                    false
                } else {
                    let tokens = document
                        .get_attribute_ns_ref(id, None, "class")?
                        .unwrap_or("");
                    wanted
                        .iter()
                        .all(|name| html_space_tokens(tokens).any(|token| token == name))
                }
            }
            Self::Name(wanted) => {
                matches!(namespace, Namespace::Html)
                    && document.get_attribute_ns_ref(id, None, "name")? == Some(wanted.as_str())
            }
            Self::WindowNamed(wanted) => {
                if wanted.is_empty() {
                    false
                } else {
                    let has_id =
                        document.get_attribute_ns_ref(id, None, "id")? == Some(wanted.as_str());
                    let has_named_legacy_element = matches!(namespace, Namespace::Html)
                        && ["embed", "form", "img", "object"]
                            .iter()
                            .any(|tag| name.eq_ignore_ascii_case(tag))
                        && document.get_attribute_ns_ref(id, None, "name")?
                            == Some(wanted.as_str());
                    has_id || has_named_legacy_element
                }
            }
            Self::FormNamedImages(form, wanted) => {
                matches!(namespace, Namespace::Html)
                    && name.as_str() == "img"
                    && lumen_html::forms::form_owner(document, id) == Some(*form)
                    && (document.get_attribute_ns_ref(id, None, "id")? == Some(wanted.as_str())
                        || document.get_attribute_ns_ref(id, None, "name")?
                            == Some(wanted.as_str()))
            }
        })
    }
}

pub(crate) fn html_space_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|character: char| matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' '))
        .filter(|token| !token.is_empty())
}

#[lumen_bind::class(name = "NodeList", hint(js(webidl)))]
pub struct DomNodeList {
    realm: Rc<DomRealm>,
    nodes: Nodes,
    _owner: Value,
    cache: RefCell<Option<CollectionCache>>,
}

struct CollectionCache {
    root: NodeId,
    version: u64,
    ids: Rc<Vec<NodeId>>,
}

impl DomNodeList {
    pub(crate) fn adopt_nodes(&mut self, realm: Rc<DomRealm>, mapping: &[(NodeId, NodeId)]) {
        self.realm = realm;
        let mapped = |node: NodeId| {
            mapping
                .iter()
                .find_map(|(old, new)| (*old == node).then_some(*new))
                .unwrap_or(node)
        };
        match &mut self.nodes {
            Nodes::Children(root, _)
            | Nodes::Descendants(root, _)
            | Nodes::SelectOptions(root)
            | Nodes::DatalistOptions(root)
            | Nodes::SelectedOptions(root)
            | Nodes::FormElements(root)
            | Nodes::FieldsetElements(root)
            | Nodes::FormElementNames(root, _)
            | Nodes::LabelAssociations(root) => *root = mapped(*root),
            Nodes::Static(nodes) => nodes.iter_mut().for_each(|node| *node = mapped(*node)),
        }
        if let Nodes::Descendants(_, DescendantFilter::FormNamedImages(form, _)) = &mut self.nodes {
            *form = mapped(*form);
        }
    }

    pub(crate) fn children(
        realm: Rc<DomRealm>,
        root: NodeId,
        elements: bool,
        owner: Value,
    ) -> Self {
        Self {
            realm,
            nodes: Nodes::Children(root, elements),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn descendants(
        realm: Rc<DomRealm>,
        root: NodeId,
        filter: DescendantFilter,
        owner: Value,
    ) -> Self {
        Self {
            realm,
            nodes: Nodes::Descendants(root, filter),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn select_options(realm: Rc<DomRealm>, root: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::SelectOptions(root),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn datalist_options(realm: Rc<DomRealm>, root: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::DatalistOptions(root),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn selected_options(realm: Rc<DomRealm>, root: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::SelectedOptions(root),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn form_elements(realm: Rc<DomRealm>, form: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::FormElements(form),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn fieldset_elements(realm: Rc<DomRealm>, fieldset: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::FieldsetElements(fieldset),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn form_element_names(
        realm: Rc<DomRealm>,
        form: NodeId,
        name: String,
        owner: Value,
    ) -> Self {
        Self {
            realm,
            nodes: Nodes::FormElementNames(form, name),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn label_associations(realm: Rc<DomRealm>, control: NodeId, owner: Value) -> Self {
        Self {
            realm,
            nodes: Nodes::LabelAssociations(control),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    pub(crate) fn form_named_images(
        realm: Rc<DomRealm>,
        form: NodeId,
        name: String,
        owner: Value,
    ) -> Self {
        Self {
            realm,
            nodes: Nodes::Descendants(form, DescendantFilter::FormNamedImages(form, name)),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    fn form_element_root(&self) -> OpResult<NodeId> {
        let Nodes::FormElements(form) = &self.nodes else {
            return Err(OpError::new(
                "InvalidStateError",
                "collection is not backed by a form",
            ));
        };
        Ok(self.realm.resolve_adopted_node(*form).1)
    }

    fn cached_ids(&self) -> OpResult<Option<(Rc<DomRealm>, Rc<Vec<NodeId>>)>> {
        let root = match &self.nodes {
            Nodes::Children(root, _) | Nodes::Descendants(root, _) => *root,
            _ => return Ok(None),
        };
        let (realm, root) = self.realm.resolve_adopted_node(root);
        let session = realm.session.borrow();
        let document = session.document();
        let version = document.version();
        if let Some(cache) = self.cache.borrow().as_ref() {
            if cache.root == root && cache.version == version {
                return Ok(Some((realm.clone(), cache.ids.clone())));
            }
        }
        let mut ids = Vec::new();
        match &self.nodes {
            Nodes::Children(_, elements) => {
                let mut node = document.first_child(root).map_err(dom_error)?;
                while let Some(id) = node {
                    if !*elements
                        || matches!(document.kind(id).map_err(dom_error)?, NodeKind::Element { .. })
                    {
                        ids.push(id);
                    }
                    node = document.next_sibling(id).map_err(dom_error)?;
                }
            }
            Nodes::Descendants(_, filter) => {
                let mut node = next_descendant(document, root, root).map_err(dom_error)?;
                while let Some(id) = node {
                    if filter.matches(document, id).map_err(dom_error)? {
                        ids.push(id);
                    }
                    node = next_descendant(document, root, id).map_err(dom_error)?;
                }
            }
            _ => return Ok(None),
        }
        let ids = Rc::new(ids);
        *self.cache.borrow_mut() = Some(CollectionCache {
            root,
            version,
            ids: ids.clone(),
        });
        drop(session);
        Ok(Some((realm, ids)))
    }

    fn visit_entries(
        &self,
        mut visit: impl FnMut(&Rc<DomRealm>, &lumen_html::Document, NodeId) -> bool,
    ) -> OpResult<()> {
        if let Some((realm, ids)) = self.cached_ids()? {
            let session = realm.session.borrow();
            for id in ids.iter().copied() {
                if !visit(&realm, session.document(), id) {
                    break;
                }
            }
            return Ok(());
        }
        match &self.nodes {
            Nodes::Static(nodes) => {
                for id in nodes.iter().copied() {
                    let (realm, id) = self.realm.resolve_adopted_node(id);
                    let session = realm.session.borrow();
                    if !visit(&realm, session.document(), id) {
                        break;
                    }
                }
            }
            Nodes::Children(root, elements) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let document = session.document();
                let mut node = document.first_child(root).map_err(dom_error)?;
                while let Some(id) = node {
                    if (!*elements
                        || matches!(
                            document.kind(id).map_err(dom_error)?,
                            NodeKind::Element { .. }
                        ))
                        && !visit(&realm, document, id)
                    {
                        break;
                    }
                    node = document.next_sibling(id).map_err(dom_error)?;
                }
            }
            Nodes::Descendants(root, filter) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let document = session.document();
                let mut node = next_descendant(document, root, root).map_err(dom_error)?;
                while let Some(id) = node {
                    if filter.matches(document, id).map_err(dom_error)?
                        && !visit(&realm, document, id)
                    {
                        break;
                    }
                    node = next_descendant(document, root, id).map_err(dom_error)?;
                }
            }
            Nodes::SelectOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                lumen_html::forms::for_each_select_option(session.document(), root, |id, _| {
                    visit(&realm, session.document(), id)
                })
                .map_err(dom_error)?;
            }
            Nodes::DatalistOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                lumen_html::forms::for_each_datalist_option(session.document(), root, |id, _| {
                    visit(&realm, session.document(), id)
                })
                .map_err(dom_error)?;
            }
            Nodes::SelectedOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let state = realm.forms.borrow();
                let session = realm.session.borrow();
                lumen_html::forms::for_each_selected_option_by(
                    session.document(),
                    root,
                    |option| super::forms::selectedness_override(&state, option),
                    |id, _, _| visit(&realm, session.document(), id),
                )
                .map_err(dom_error)?;
            }
            Nodes::FormElements(form) => {
                let (realm, form) = self.realm.resolve_adopted_node(*form);
                let session = realm.session.borrow();
                lumen_html::forms::for_each_form_element(session.document(), form, |id, _| {
                    visit(&realm, session.document(), id)
                })
                .map_err(dom_error)?;
            }
            Nodes::FieldsetElements(fieldset) => {
                let (realm, fieldset) = self.realm.resolve_adopted_node(*fieldset);
                let session = realm.session.borrow();
                lumen_html::forms::for_each_fieldset_element(
                    session.document(),
                    fieldset,
                    |id, _| visit(&realm, session.document(), id),
                )
                .map_err(dom_error)?;
            }
            Nodes::FormElementNames(form, name) => {
                let (realm, form) = self.realm.resolve_adopted_node(*form);
                let session = realm.session.borrow();
                let document = session.document();
                let mut error = None;
                lumen_html::forms::for_each_form_element(document, form, |id, _| {
                    let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
                        Ok(Some(value)) => value == name.as_str(),
                        Ok(None) => false,
                        Err(value) => {
                            error = Some(value);
                            return false;
                        }
                    };
                    let name_matches = match document.get_attribute_ns_ref(id, None, "name") {
                        Ok(Some(value)) => value == name.as_str(),
                        Ok(None) => false,
                        Err(value) => {
                            error = Some(value);
                            return false;
                        }
                    };
                    if id_matches || name_matches {
                        visit(&realm, document, id)
                    } else {
                        true
                    }
                })
                .map_err(dom_error)?;
                if let Some(error) = error {
                    return Err(dom_error(error));
                }
            }
            Nodes::LabelAssociations(control) => {
                let (realm, control) = self.realm.resolve_adopted_node(*control);
                let session = realm.session.borrow();
                lumen_html::labels::for_each_control_label(session.document(), control, |id| {
                    visit(&realm, session.document(), id)
                })
                .map_err(dom_error)?;
            }
        }
        Ok(())
    }
    pub(crate) fn snapshot(realm: Rc<DomRealm>, nodes: Vec<NodeId>, owner: Value) -> Self {
        for id in &nodes {
            *realm.retained_nodes.borrow_mut().entry(*id).or_default() += 1;
        }
        Self {
            realm,
            nodes: Nodes::Static(nodes),
            _owner: owner,
            cache: RefCell::new(None),
        }
    }
    fn node_at(&self, index: usize) -> OpResult<Option<(Rc<DomRealm>, NodeId)>> {
        if let Some((realm, ids)) = self.cached_ids()? {
            return Ok(ids.get(index).map(|id| (realm, *id)));
        }
        match &self.nodes {
            Nodes::Static(nodes) => Ok(nodes
                .get(index)
                .copied()
                .map(|node| self.realm.resolve_adopted_node(node))),
            Nodes::Children(root, elements) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let document = session.document();
                let mut node = document.first_child(root).map_err(dom_error)?;
                let mut count = 0;
                while let Some(id) = node {
                    if !*elements
                        || matches!(
                            document.kind(id).map_err(dom_error)?,
                            NodeKind::Element { .. }
                        )
                    {
                        if count == index {
                            return Ok(Some((realm.clone(), id)));
                        }
                        count += 1;
                    }
                    node = document.next_sibling(id).map_err(dom_error)?;
                }
                Ok(None)
            }
            Nodes::SelectOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                Ok(
                    lumen_html::forms::select_option_at(session.document(), root, index)
                        .map_err(dom_error)?
                        .map(|id| (realm.clone(), id)),
                )
            }
            Nodes::DatalistOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                Ok(
                    lumen_html::forms::datalist_option_at(session.document(), root, index)
                        .map_err(dom_error)?
                        .map(|id| (realm.clone(), id)),
                )
            }
            Nodes::SelectedOptions(_) => {
                let mut count = 0;
                let mut found = None;
                self.visit_entries(|realm, _, id| {
                    if count == index {
                        found = Some((realm.clone(), id));
                        false
                    } else {
                        count += 1;
                        true
                    }
                })?;
                Ok(found)
            }
            Nodes::FormElements(form) => {
                let (realm, form) = self.realm.resolve_adopted_node(*form);
                let session = realm.session.borrow();
                Ok(
                    lumen_html::forms::form_element_at(session.document(), form, index)
                        .map_err(dom_error)?
                        .map(|id| (realm.clone(), id)),
                )
            }
            Nodes::FieldsetElements(fieldset) => {
                let (realm, fieldset) = self.realm.resolve_adopted_node(*fieldset);
                let session = realm.session.borrow();
                Ok(
                    lumen_html::forms::fieldset_element_at(session.document(), fieldset, index)
                        .map_err(dom_error)?
                        .map(|id| (realm.clone(), id)),
                )
            }
            Nodes::FormElementNames(_, _) => {
                let mut count = 0;
                let mut found = None;
                self.visit_entries(|realm, _, id| {
                    if count == index {
                        found = Some((realm.clone(), id));
                        false
                    } else {
                        count += 1;
                        true
                    }
                })?;
                Ok(found)
            }
            Nodes::LabelAssociations(_) => {
                let mut count = 0;
                let mut found = None;
                self.visit_entries(|realm, _, id| {
                    if count == index {
                        found = Some((realm.clone(), id));
                        false
                    } else {
                        count += 1;
                        true
                    }
                })?;
                Ok(found)
            }
            Nodes::Descendants(_, _) => {
                let mut count = 0;
                let mut found = None;
                self.visit_entries(|realm, _, id| {
                    if count == index {
                        found = Some((realm.clone(), id));
                        false
                    } else {
                        count += 1;
                        true
                    }
                })?;
                Ok(found)
            }
        }
    }
    fn entries(&self) -> OpResult<Vec<(Rc<DomRealm>, NodeId)>> {
        if let Some((realm, ids)) = self.cached_ids()? {
            return Ok(ids.iter().map(|id| (realm.clone(), *id)).collect());
        }
        match &self.nodes {
            Nodes::Static(nodes) => Ok(nodes
                .iter()
                .copied()
                .map(|node| self.realm.resolve_adopted_node(node))
                .collect()),
            Nodes::Children(root, elements) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let mut ids = children(session.document(), root).map_err(dom_error)?;
                if *elements {
                    ids.retain(|id| {
                        matches!(session.document().kind(*id), Ok(NodeKind::Element { .. }))
                    });
                }
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
            Nodes::SelectOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                Ok(lumen_html::forms::select_options(session.document(), root)
                    .into_iter()
                    .map(|id| (realm.clone(), id))
                    .collect())
            }
            Nodes::DatalistOptions(root) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let mut ids = Vec::new();
                lumen_html::forms::for_each_datalist_option(session.document(), root, |id, _| {
                    ids.push(id);
                    true
                })
                .map_err(dom_error)?;
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
            Nodes::SelectedOptions(_) => {
                let mut nodes = Vec::new();
                self.visit_entries(|realm, _, id| {
                    nodes.push((realm.clone(), id));
                    true
                })?;
                Ok(nodes)
            }
            Nodes::FormElements(form) => {
                let (realm, form) = self.realm.resolve_adopted_node(*form);
                let session = realm.session.borrow();
                let mut ids = Vec::new();
                lumen_html::forms::for_each_form_element(session.document(), form, |id, _| {
                    ids.push(id);
                    true
                })
                .map_err(dom_error)?;
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
            Nodes::FieldsetElements(fieldset) => {
                let (realm, fieldset) = self.realm.resolve_adopted_node(*fieldset);
                let session = realm.session.borrow();
                let mut ids = Vec::new();
                lumen_html::forms::for_each_fieldset_element(
                    session.document(),
                    fieldset,
                    |id, _| {
                        ids.push(id);
                        true
                    },
                )
                .map_err(dom_error)?;
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
            Nodes::FormElementNames(form, name) => {
                let (realm, form) = self.realm.resolve_adopted_node(*form);
                let session = realm.session.borrow();
                let document = session.document();
                let mut ids = Vec::new();
                let mut error = None;
                lumen_html::forms::for_each_form_element(document, form, |id, _| {
                    let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
                        Ok(Some(value)) => value == name.as_str(),
                        Ok(None) => false,
                        Err(value) => {
                            error = Some(value);
                            return false;
                        }
                    };
                    let name_matches = match document.get_attribute_ns_ref(id, None, "name") {
                        Ok(Some(value)) => value == name.as_str(),
                        Ok(None) => false,
                        Err(value) => {
                            error = Some(value);
                            return false;
                        }
                    };
                    if id_matches || name_matches {
                        ids.push(id);
                    }
                    true
                })
                .map_err(dom_error)?;
                if let Some(error) = error {
                    return Err(dom_error(error));
                }
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
            Nodes::LabelAssociations(_) => {
                let mut nodes = Vec::new();
                self.visit_entries(|realm, _, id| {
                    nodes.push((realm.clone(), id));
                    true
                })?;
                Ok(nodes)
            }
            Nodes::Descendants(root, filter) => {
                let (realm, root) = self.realm.resolve_adopted_node(*root);
                let session = realm.session.borrow();
                let document = session.document();
                let mut ids = Vec::new();
                let mut node = next_descendant(document, root, root).map_err(dom_error)?;
                while let Some(id) = node {
                    if filter.matches(document, id).map_err(dom_error)? {
                        ids.push(id);
                    }
                    node = next_descendant(document, root, id).map_err(dom_error)?;
                }
                Ok(ids.into_iter().map(|id| (realm.clone(), id)).collect())
            }
        }
    }

    fn find_html_collection_item(
        &self,
        name: &str,
        all_name_attributes: bool,
    ) -> OpResult<Option<(Rc<DomRealm>, NodeId)>> {
        if name.is_empty() {
            return Ok(None);
        }
        let mut found = None;
        let mut error = None;
        self.visit_entries(|realm, document, id| {
            let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
                Ok(Some(value)) => value == name,
                Ok(None) => false,
                Err(value) => {
                    error = Some(value);
                    return false;
                }
            };
            let name_matches = if all_name_attributes || legacy_name_attribute(document, id) {
                match document.get_attribute_ns_ref(id, None, "name") {
                    Ok(Some(value)) => value == name,
                    Ok(None) => false,
                    Err(value) => {
                        error = Some(value);
                        return false;
                    }
                }
            } else {
                false
            };
            if id_matches || name_matches {
                found = Some((realm.clone(), id));
                false
            } else {
                true
            }
        })?;
        if let Some(error) = error {
            return Err(dom_error(error));
        }
        Ok(found)
    }

    fn named_property_names(&self, all_name_attributes: bool) -> OpResult<Vec<String>> {
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut error = None;
        self.visit_entries(|_, document, id| {
            match document.get_attribute_ns_ref(id, None, "id") {
                Ok(Some(value)) if !value.is_empty() && seen.insert(value.to_owned()) => {
                    names.push(value.to_owned());
                }
                Ok(_) => {}
                Err(value) => {
                    error = Some(value);
                    return false;
                }
            }
            if all_name_attributes || legacy_name_attribute(document, id) {
                match document.get_attribute_ns_ref(id, None, "name") {
                    Ok(Some(value)) if !value.is_empty() && seen.insert(value.to_owned()) => {
                        names.push(value.to_owned());
                    }
                    Ok(_) => {}
                    Err(value) => {
                        error = Some(value);
                        return false;
                    }
                }
            }
            true
        })?;
        if let Some(error) = error {
            return Err(dom_error(error));
        }
        Ok(names)
    }

    pub(crate) fn supports_html_collection_name(
        &self,
        name: &str,
        all_name_attributes: bool,
    ) -> OpResult<bool> {
        if name.is_empty() {
            return Ok(false);
        }
        Ok(self
            .find_html_collection_item(name, all_name_attributes)?
            .is_some())
    }

    pub(crate) fn html_collection_names(&self, all_name_attributes: bool) -> OpResult<Vec<String>> {
        self.named_property_names(all_name_attributes)
    }

    pub(crate) fn html_collection_named_item(
        &self,
        ctx: &mut Ctx,
        name: &str,
        all_name_attributes: bool,
    ) -> OpResult<Value> {
        if name.is_empty() {
            return Ok(Value::Null);
        }
        let found = self.find_html_collection_item(name, all_name_attributes)?;
        Ok(found.map_or(Value::Null, |(realm, id)| realm.wrap(ctx, id)))
    }
}

fn legacy_name_attribute(document: &lumen_html::Document, id: NodeId) -> bool {
    let Ok(NodeKind::Element {
        namespace: Namespace::Html,
        name,
        ..
    }) = document.kind(id)
    else {
        return false;
    };
    let local_name = lumen_html::svg::local_name(&name);
    [
        "a", "applet", "area", "embed", "form", "frame", "frameset", "iframe", "img", "object",
    ]
    .iter()
    .any(|candidate| local_name.eq_ignore_ascii_case(candidate))
}

impl Drop for DomNodeList {
    fn drop(&mut self) {
        if let Nodes::Static(nodes) = &self.nodes {
            for id in nodes {
                let (realm, id) = self.realm.resolve_adopted_node(*id);
                let mut retained = realm.retained_nodes.borrow_mut();
                if let Some(count) = retained.get_mut(&id) {
                    *count -= 1;
                    if *count == 0 {
                        retained.remove(&id);
                    }
                }
            }
        }
    }
}

#[lumen_bind::methods]
impl DomNodeList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        if let Some((_, ids)) = self.cached_ids()? {
            return Ok(ids.len());
        }
        if let Nodes::SelectOptions(root) = &self.nodes {
            let (realm, root) = self.realm.resolve_adopted_node(*root);
            let session = realm.session.borrow();
            return lumen_html::forms::select_option_count(session.document(), root)
                .map_err(dom_error);
        }
        if let Nodes::DatalistOptions(root) = &self.nodes {
            let (realm, root) = self.realm.resolve_adopted_node(*root);
            let session = realm.session.borrow();
            return lumen_html::forms::datalist_option_count(session.document(), root)
                .map_err(dom_error);
        }
        if let Nodes::FormElements(form) = &self.nodes {
            let (realm, form) = self.realm.resolve_adopted_node(*form);
            let session = realm.session.borrow();
            return lumen_html::forms::form_element_count(session.document(), form)
                .map_err(dom_error);
        }
        if let Nodes::FieldsetElements(fieldset) = &self.nodes {
            let (realm, fieldset) = self.realm.resolve_adopted_node(*fieldset);
            let session = realm.session.borrow();
            return lumen_html::forms::fieldset_element_count(session.document(), fieldset)
                .map_err(dom_error);
        }
        if matches!(&self.nodes, Nodes::FormElementNames(_, _)) {
            let mut count = 0;
            self.visit_entries(|_, _, _| {
                count += 1;
                true
            })?;
            return Ok(count);
        }
        let mut count = 0;
        self.visit_entries(|_, _, _| {
            count += 1;
            true
        })?;
        Ok(count)
    }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self
            .node_at(index)?
            .map_or(Value::Undefined, |(realm, id)| realm.wrap(ctx, id)))
    }
    #[method(coerce)]
    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self
            .node_at(index)?
            .map_or(Value::Null, |(realm, id)| realm.wrap(ctx, id)))
    }
    fn for_each(
        &self,
        ctx: &mut Ctx,
        this: This<Value>,
        callback: lumen::embed::JsFunction,
        this_arg: Option<Value>,
    ) -> OpResult<()> {
        for (index, (realm, id)) in self.entries()?.into_iter().enumerate() {
            let value = realm.wrap(ctx, id);
            callback.call(
                ctx,
                this_arg.clone().unwrap_or(Value::Undefined),
                &[value, Value::Num(index as f64), this.0.clone()],
            )?;
        }
        Ok(())
    }
    #[proto(iter)]
    fn values(&self, this: This<Value>) -> DomCollectionIterator {
        DomCollectionIterator {
            collection: this.0,
            index: Cell::new(0),
        }
    }
}

#[lumen_bind::class(name = "HTMLCollection", hint(js(webidl, named_properties)))]
pub struct DomHtmlCollection {
    pub(crate) base: DomNodeList,
}

#[lumen_bind::methods]
impl DomHtmlCollection {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> OpResult<bool> {
        self.base.supports_html_collection_name(name, false)
    }

    #[method(hint(js(named_names)))]
    fn named_names(&self) -> OpResult<Vec<String>> {
        self.base.html_collection_names(false)
    }

    #[method(hint(js(named_getter)))]
    fn named_getter(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        self.named_item(ctx, name)
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        self.base.length()
    }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        self.base.indexed(ctx, index)
    }
    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        self.base.item(ctx, index)
    }
    #[proto(iter)]
    fn values(&self, this: This<Value>) -> DomCollectionIterator {
        DomCollectionIterator {
            collection: this.0,
            index: Cell::new(0),
        }
    }
    #[method(coerce)]
    fn named_item(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        self.base.html_collection_named_item(ctx, name, false)
    }
}

#[lumen_bind::class(
    name = "HTMLFormControlsCollection",
    extends = DomHtmlCollection,
    hint(js(webidl, named_properties))
)]
pub struct DomHtmlFormControlsCollection {
    pub(crate) base: DomHtmlCollection,
}

#[lumen_bind::methods]
impl DomHtmlFormControlsCollection {
    #[method(hint(js(named_supported)))]
    fn named_supported(&self, name: &str) -> OpResult<bool> {
        self.base.base.supports_html_collection_name(name, true)
    }

    #[method(hint(js(named_names)))]
    fn named_names(&self) -> OpResult<Vec<String>> {
        self.base.base.html_collection_names(true)
    }

    #[method(hint(js(named_getter)))]
    fn named_getter(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        self.named_item(ctx, name)
    }

    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        self.base.length()
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        self.base.indexed(ctx, index)
    }

    #[method(coerce)]
    fn named_item(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        if name.is_empty() {
            return Ok(Value::Null);
        }
        let mut count = 0usize;
        let mut first = None;
        let mut error = None;
        self.base.base.visit_entries(|realm, document, id| {
            let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
                Ok(Some(value)) => value == name,
                Ok(None) => false,
                Err(value) => {
                    error = Some(value);
                    return false;
                }
            };
            let name_matches = match document.get_attribute_ns_ref(id, None, "name") {
                Ok(Some(value)) => value == name,
                Ok(None) => false,
                Err(value) => {
                    error = Some(value);
                    return false;
                }
            };
            if id_matches || name_matches {
                count += 1;
                if first.is_none() {
                    first = Some((realm.clone(), id));
                }
            }
            true
        })?;
        if let Some(error) = error {
            return Err(dom_error(error));
        }
        match (count, first) {
            (0, _) => Ok(Value::Null),
            (1, Some((realm, id))) => Ok(realm.wrap(ctx, id)),
            (_, Some((realm, _))) => Ok(ctx.new_instance(DomRadioNodeList {
                base: DomNodeList::form_element_names(
                    realm,
                    self.base.base.form_element_root()?,
                    name.to_owned(),
                    self.base.base._owner.clone(),
                ),
            })),
            _ => Ok(Value::Null),
        }
    }
}

fn current_form_named_matches(
    document: &lumen_html::Document,
    form: NodeId,
    name: &str,
) -> OpResult<(usize, Option<NodeId>)> {
    let mut count = 0usize;
    let mut first = None;
    let mut error = None;
    lumen_html::forms::for_each_form_element(document, form, |id, _| {
        let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
            Ok(Some(value)) => value == name,
            Ok(None) => false,
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        let name_matches = match document.get_attribute_ns_ref(id, None, "name") {
            Ok(Some(value)) => value == name,
            Ok(None) => false,
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        if id_matches || name_matches {
            count += 1;
            first.get_or_insert(id);
        }
        true
    })
    .map_err(dom_error)?;
    if let Some(error) = error {
        return Err(dom_error(error));
    }
    Ok((count, first))
}

fn current_form_named_images_matches(
    document: &lumen_html::Document,
    form: NodeId,
    name: &str,
) -> OpResult<(usize, Option<NodeId>)> {
    let mut count = 0usize;
    let mut first = None;
    let mut error = None;
    lumen_html::forms::for_each_form_named_image(document, form, |id| {
        let id_matches = match document.get_attribute_ns_ref(id, None, "id") {
            Ok(Some(value)) => value == name,
            Ok(None) => false,
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        let name_matches = match document.get_attribute_ns_ref(id, None, "name") {
            Ok(Some(value)) => value == name,
            Ok(None) => false,
            Err(value) => {
                error = Some(value);
                return false;
            }
        };
        if id_matches || name_matches {
            count += 1;
            first.get_or_insert(id);
        }
        true
    })
    .map_err(dom_error)?;
    if let Some(error) = error {
        return Err(dom_error(error));
    }
    Ok((count, first))
}

fn prune_past_form_names(realm: &Rc<DomRealm>, form: NodeId) {
    let session = realm.session.borrow();
    let document = session.document();
    realm
        .forms
        .borrow_mut()
        .forget_form_names_not_owned_by(form, |node| {
            lumen_html::forms::form_owner(document, node) == Some(form)
        });
}

pub(crate) fn form_named_property_supported(
    realm: &Rc<DomRealm>,
    form: NodeId,
    name: &str,
) -> OpResult<bool> {
    if name.is_empty() {
        return Ok(false);
    }
    prune_past_form_names(realm, form);
    let session = realm.session.borrow();
    let document = session.document();
    if current_form_named_matches(document, form, name)?.0 != 0 {
        return Ok(true);
    }
    if current_form_named_images_matches(document, form, name)?.0 != 0 {
        return Ok(true);
    }
    Ok(realm.forms.borrow().past_form_name(form, name).is_some())
}

pub(crate) fn form_named_property_names(
    realm: &Rc<DomRealm>,
    form: NodeId,
) -> OpResult<Vec<String>> {
    prune_past_form_names(realm, form);
    let session = realm.session.borrow();
    let document = session.document();
    let mut names = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut error = None;
    let state = realm.forms.borrow();
    let past_names = state.past_form_names(form);
    lumen_html::forms::for_each_form_named_property_element(document, form, |id, kind| {
        if kind != lumen_html::forms::FormNamedElementKind::Other {
            for attribute in ["id", "name"] {
                match document.get_attribute_ns_ref(id, None, attribute) {
                    Ok(Some(value)) if !value.is_empty() && seen.insert(value.to_owned()) => {
                        names.push(value.to_owned());
                    }
                    Ok(_) => {}
                    Err(value) => {
                        error = Some(value);
                        return false;
                    }
                }
            }
        }
        for (name, past_node) in past_names {
            if *past_node == id && !name.is_empty() && seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        true
    })
    .map_err(dom_error)?;
    if let Some(error) = error {
        return Err(dom_error(error));
    }
    Ok(names)
}

pub(crate) fn form_named_property_getter(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    form: NodeId,
    owner: Value,
    name: &str,
) -> OpResult<Value> {
    if name.is_empty() {
        return Ok(Value::Undefined);
    }
    prune_past_form_names(realm, form);
    let (count, first) = {
        let session = realm.session.borrow();
        current_form_named_matches(session.document(), form, name)?
    };
    match (count, first) {
        (1, Some(node)) => {
            realm
                .forms
                .borrow_mut()
                .remember_form_name(form, name, node);
            return Ok(realm.wrap(ctx, node));
        }
        (count, Some(_)) if count > 1 => {
            return Ok(ctx.new_instance(DomRadioNodeList {
                base: DomNodeList::form_element_names(
                    realm.clone(),
                    form,
                    name.to_owned(),
                    owner.clone(),
                ),
            }));
        }
        _ => {}
    }
    let (image_count, first_image) = {
        let session = realm.session.borrow();
        current_form_named_images_matches(session.document(), form, name)?
    };
    match (image_count, first_image) {
        (1, Some(node)) => {
            realm
                .forms
                .borrow_mut()
                .remember_form_name(form, name, node);
            Ok(realm.wrap(ctx, node))
        }
        (count, Some(_)) if count > 1 => Ok(ctx.new_instance(DomRadioNodeList {
            base: DomNodeList::form_named_images(realm.clone(), form, name.to_owned(), owner),
        })),
        (0, _) => {
            let old = realm.forms.borrow().past_form_name(form, name);
            Ok(old.map_or(Value::Undefined, |node| realm.wrap(ctx, node)))
        }
        _ => Ok(Value::Undefined),
    }
}

#[lumen_bind::class(name = "RadioNodeList", extends = DomNodeList, hint(js(webidl)))]
pub struct DomRadioNodeList {
    pub(crate) base: DomNodeList,
}

#[lumen_bind::methods]
impl DomRadioNodeList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        let mut count = 0;
        self.base.visit_entries(|_, _, _| {
            count += 1;
            true
        })?;
        Ok(count)
    }

    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self
            .base
            .node_at(index)?
            .map_or(Value::Undefined, |(realm, id)| realm.wrap(ctx, id)))
    }

    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self
            .base
            .node_at(index)?
            .map_or(Value::Null, |(realm, id)| realm.wrap(ctx, id)))
    }

    #[getter]
    fn value(&self) -> OpResult<String> {
        for (realm, id) in self.base.entries()? {
            let is_radio = {
                let session = realm.session.borrow();
                matches!(
                    session.document().kind(id),
                    Ok(NodeKind::Element { namespace: Namespace::Html, name, .. })
                        if name.as_str() == "input"
                            && session.document()
                                .get_attribute_ns_ref(id, None, "type")
                                .ok()
                                .flatten()
                                .unwrap_or("text")
                                .eq_ignore_ascii_case("radio")
                )
            };
            if is_radio && forms::checked(&realm, id)? {
                return forms::control_value(&realm, id);
            }
        }
        Ok(String::new())
    }

    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        for (realm, id) in self.base.entries()? {
            let is_radio = {
                let session = realm.session.borrow();
                matches!(
                    session.document().kind(id),
                    Ok(NodeKind::Element { namespace: Namespace::Html, name, .. })
                        if name.as_str() == "input"
                            && session.document()
                                .get_attribute_ns_ref(id, None, "type")
                                .ok()
                                .flatten()
                                .unwrap_or("text")
                                .eq_ignore_ascii_case("radio")
                )
            };
            if is_radio && forms::control_value(&realm, id)? == value {
                forms::set_checked(&realm, &mut realm.forms.borrow_mut(), id, true)?;
                break;
            }
        }
        Ok(())
    }
}

#[lumen_bind::class(name = "DOMCollectionIterator")]
pub struct DomCollectionIterator {
    collection: Value,
    index: Cell<usize>,
}

#[lumen_bind::methods]
impl DomCollectionIterator {
    #[proto(iter)]
    fn iter(&self, this: This<Value>) -> Value {
        this.0
    }
    #[proto(next)]
    fn next(&self, ctx: &mut Ctx) -> lumen::embed::OpResult<Option<Value>> {
        let length = ctx
            .get_member(&self.collection, "length")
            .map_err(|_| lumen::embed::OpError::new("TypeError", "collection length failed"))?;
        let Value::Num(length) = length else {
            return Err(lumen::embed::OpError::new(
                "TypeError",
                "invalid collection length",
            ));
        };
        let index = self.index.get();
        if index >= length as usize {
            return Ok(None);
        }
        let value = ctx
            .get_member(&self.collection, &index.to_string())
            .map_err(|_| lumen::embed::OpError::new("TypeError", "collection read failed"))?;
        self.index.set(index + 1);
        Ok(Some(value))
    }
}

#[lumen_bind::class(name = "DOMTokenList", hint(js(webidl)))]
pub struct DomTokenList {
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) node: NodeId,
    pub(crate) owner: Value,
}

impl DomTokenList {
    pub(crate) fn adopt_node(&mut self, realm: Rc<DomRealm>, node: NodeId) {
        self.realm = realm;
        self.node = node;
    }

    fn tokens(&self) -> OpResult<Vec<String>> {
        let session = self.realm.session.borrow();
        let NodeKind::Element { .. } = session.document().kind(self.node).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "classList requires an element"));
        };
        let mut tokens = Vec::new();
        let class_value = session
            .document()
            .get_attribute_ns_ref(self.node, None, "class")
            .map_err(dom_error)?
            .unwrap_or("");
        for token in html_space_tokens(class_value) {
            if !tokens.iter().any(|old| old == token) {
                tokens.push(token.to_owned());
            }
        }
        Ok(tokens)
    }
    fn validate(token: &str) -> OpResult<()> {
        if token.is_empty() {
            return Err(OpError::new("SyntaxError", "token must be nonempty"));
        }
        if token
            .chars()
            .any(|character| matches!(character, '\t' | '\n' | '\u{c}' | '\r' | ' '))
        {
            return Err(OpError::new(
                "InvalidCharacterError",
                "token must contain no ASCII whitespace",
            ));
        }
        Ok(())
    }
    fn write(&self, tokens: &[String]) -> OpResult<()> {
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.node, None, "class", &tokens.join(" "))
            .map_err(dom_error)
    }
}

#[lumen_bind::methods]
impl DomTokenList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        Ok(self.tokens()?.len())
    }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Option<String>> {
        Ok(self.tokens()?.get(index).cloned())
    }
    fn item(&self, index: usize) -> OpResult<Option<String>> {
        self.indexed(index)
    }
    #[method(coerce)]
    fn contains(&self, token: &str) -> OpResult<bool> {
        Self::validate(token)?;
        Ok(self.tokens()?.iter().any(|old| old == token))
    }
    #[method(coerce)]
    fn add(&self, #[varargs] tokens: Vec<String>) -> OpResult<()> {
        for token in &tokens {
            Self::validate(token)?;
        }
        let mut current = self.tokens()?;
        for token in tokens {
            if !current.contains(&token) {
                current.push(token);
            }
        }
        self.write(&current)
    }
    #[method(coerce)]
    fn remove(&self, #[varargs] tokens: Vec<String>) -> OpResult<()> {
        for token in &tokens {
            Self::validate(token)?;
        }
        let mut current = self.tokens()?;
        current.retain(|token| !tokens.contains(token));
        self.write(&current)
    }
    #[method(coerce)]
    fn toggle(&self, token: &str, force: Option<bool>) -> OpResult<bool> {
        Self::validate(token)?;
        let mut current = self.tokens()?;
        let found = current.iter().any(|old| old == token);
        let add = force.unwrap_or(!found);
        if add == found {
            return Ok(found);
        }
        if add && !found {
            current.push(token.into());
        }
        if !add {
            current.retain(|old| old != token);
        }
        self.write(&current)?;
        Ok(add)
    }
    #[getter]
    fn value(&self) -> OpResult<String> {
        Ok(self.tokens()?.join(" "))
    }
    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute_ns(self.node, None, "class", value)
            .map_err(dom_error)
    }
    #[proto(iter)]
    fn values(&self, this: This<Value>) -> DomCollectionIterator {
        DomCollectionIterator {
            collection: this.0,
            index: Cell::new(0),
        }
    }
}
