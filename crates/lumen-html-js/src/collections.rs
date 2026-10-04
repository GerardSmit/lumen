use super::*;
use lumen_bind::This;

#[derive(Clone)]
enum Nodes {
    Children(NodeId, bool),
    Descendants(NodeId, DescendantFilter),
    Static(Vec<NodeId>),
}

#[derive(Clone)]
pub(crate) enum DescendantFilter {
    Tag(String),
    TagNs(Option<String>, String),
    Class(Vec<String>),
    WindowNamed(String),
}

impl DescendantFilter {
    pub(crate) fn matches(
        &self,
        document: &lumen_html::Document,
        id: NodeId,
    ) -> Result<bool, Error> {
        let NodeKind::Element {
            namespace,
            name,
            attributes,
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
                    let tokens = attributes
                        .iter()
                        .find(|(key, _)| key == "class")
                        .map_or("", |(_, value)| value);
                    wanted
                        .iter()
                        .all(|name| html_space_tokens(tokens).any(|token| token == name))
                }
            }
            Self::WindowNamed(wanted) => {
                if wanted.is_empty() {
                    false
                } else {
                    let has_id = attributes
                        .iter()
                        .any(|(key, value)| key == "id" && value == wanted);
                    let has_named_legacy_element = matches!(namespace, Namespace::Html)
                        && ["embed", "form", "img", "object"]
                            .iter()
                            .any(|tag| name.eq_ignore_ascii_case(tag))
                        && attributes
                            .iter()
                            .any(|(key, value)| key == "name" && value == wanted);
                    has_id || has_named_legacy_element
                }
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
            Nodes::Children(root, _) | Nodes::Descendants(root, _) => *root = mapped(*root),
            Nodes::Static(nodes) => nodes.iter_mut().for_each(|node| *node = mapped(*node)),
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
        }
    }
    pub(crate) fn snapshot(realm: Rc<DomRealm>, nodes: Vec<NodeId>, owner: Value) -> Self {
        for id in &nodes {
            *realm.retained_nodes.borrow_mut().entry(*id).or_default() += 1;
        }
        Self {
            realm,
            nodes: Nodes::Static(nodes),
            _owner: owner,
        }
    }
    fn node_at(&self, index: usize) -> OpResult<Option<(Rc<DomRealm>, NodeId)>> {
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
            Nodes::Descendants(_, _) => Ok(self.entries()?.get(index).cloned()),
        }
    }
    fn entries(&self) -> OpResult<Vec<(Rc<DomRealm>, NodeId)>> {
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
        Ok(self.entries()?.len())
    }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self
            .node_at(index)?
            .map_or(Value::Undefined, |(realm, id)| realm.wrap(ctx, id)))
    }
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

#[lumen_bind::class(name = "HTMLCollection", extends = DomNodeList, hint(js(webidl)))]
pub struct DomHtmlCollection {
    pub(crate) base: DomNodeList,
}

#[lumen_bind::methods]
impl DomHtmlCollection {
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
        let ids = self.base.entries()?;
        let found =
            ids.into_iter().find(
                |(realm, id)| match realm.session.borrow().document().kind(*id) {
                    Ok(NodeKind::Element { attributes, .. }) if !name.is_empty() => attributes
                        .iter()
                        .any(|(key, value)| (key == "id" || key == "name") && value == name),
                    _ => false,
                },
            );
        Ok(found.map_or(Value::Null, |(realm, id)| realm.wrap(ctx, id)))
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
        let NodeKind::Element { attributes, .. } =
            session.document().kind(self.node).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "classList requires an element"));
        };
        let mut tokens = Vec::new();
        let class_value = attributes
            .iter()
            .find(|(name, _)| name == "class")
            .map_or("", |(_, value)| value);
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
            .set_attribute(self.node, "class", &tokens.join(" "))
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
            .set_attribute(self.node, "class", value)
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
