use super::*;
use lumen_bind::This;

#[derive(Clone)]
enum Nodes { Children(NodeId, bool), Static(Vec<NodeId>) }

#[lumen_bind::class(name = "NodeList")]
pub struct DomNodeList {
    realm: Rc<DomRealm>,
    nodes: Nodes,
    _owner: Value,
}

impl DomNodeList {
    pub(crate) fn children(realm: Rc<DomRealm>, root: NodeId, elements: bool, owner: Value) -> Self { Self { realm, nodes: Nodes::Children(root, elements), _owner: owner } }
    pub(crate) fn snapshot(realm: Rc<DomRealm>, nodes: Vec<NodeId>, owner: Value) -> Self {
        for id in &nodes { *realm.retained_nodes.borrow_mut().entry(*id).or_default() += 1; }
        Self { realm, nodes: Nodes::Static(nodes), _owner: owner }
    }
    fn node_at(&self, index: usize) -> OpResult<Option<NodeId>> {
        match &self.nodes {
            Nodes::Static(nodes) => Ok(nodes.get(index).copied()),
            Nodes::Children(root, elements) => {
                let session = self.realm.session.borrow();
                let document = session.document();
                let mut node = document.first_child(*root).map_err(dom_error)?;
                let mut count = 0;
                while let Some(id) = node {
                    if !*elements || matches!(document.kind(id).map_err(dom_error)?, NodeKind::Element { .. }) {
                        if count == index { return Ok(Some(id)); }
                        count += 1;
                    }
                    node = document.next_sibling(id).map_err(dom_error)?;
                }
                Ok(None)
            }
        }
    }
    fn ids(&self) -> OpResult<Vec<NodeId>> {
        match &self.nodes {
            Nodes::Static(nodes) => Ok(nodes.clone()),
            Nodes::Children(root, elements) => {
                let session = self.realm.session.borrow();
                let mut ids = children(session.document(), *root).map_err(dom_error)?;
                if *elements { ids.retain(|id| matches!(session.document().kind(*id), Ok(NodeKind::Element { .. }))); }
                Ok(ids)
            }
        }
    }
}

impl Drop for DomNodeList {
    fn drop(&mut self) {
        if let Nodes::Static(nodes) = &self.nodes {
            let mut retained = self.realm.retained_nodes.borrow_mut();
            for id in nodes { if let Some(count) = retained.get_mut(id) { *count -= 1; if *count == 0 { retained.remove(id); } } }
        }
    }
}

#[lumen_bind::methods]
impl DomNodeList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> {
        if let Nodes::Static(nodes) = &self.nodes { return Ok(nodes.len()); }
        let Nodes::Children(root, elements) = self.nodes else { unreachable!() };
        let session = self.realm.session.borrow();
        let document = session.document();
        let mut node = document.first_child(root).map_err(dom_error)?;
        let mut count = 0;
        while let Some(id) = node { if !elements || matches!(document.kind(id).map_err(dom_error)?, NodeKind::Element { .. }) { count += 1; } node = document.next_sibling(id).map_err(dom_error)?; }
        Ok(count)
    }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self.node_at(index)?.map_or(Value::Undefined, |id| self.realm.wrap(ctx, id)))
    }
    fn item(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> {
        Ok(self.realm.wrap_option(ctx, self.node_at(index)?))
    }
    #[proto(iter)]
    fn values(&self, this: This<Value>) -> DomCollectionIterator { DomCollectionIterator { collection: this.0, index: Cell::new(0) } }
}

#[lumen_bind::class(name = "HTMLCollection", extends = DomNodeList)]
pub struct DomHtmlCollection { pub(crate) base: DomNodeList }

#[lumen_bind::methods]
impl DomHtmlCollection {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> { self.base.length() }
    #[proto(getitem)]
    fn indexed(&self, ctx: &mut Ctx, index: usize) -> OpResult<Value> { self.base.indexed(ctx, index) }
    fn named_item(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        let session = self.base.realm.session.borrow();
        let ids = self.base.ids()?;
        let id = ids.into_iter().find(|id| match session.document().kind(*id) { Ok(NodeKind::Element { attributes, .. }) => attributes.iter().any(|(key, value)| (key == "id" || key == "name") && value == name), _ => false });
        drop(session);
        Ok(self.base.realm.wrap_option(ctx, id))
    }
}

#[lumen_bind::class(name = "DOMCollectionIterator")]
pub struct DomCollectionIterator { collection: Value, index: Cell<usize> }

#[lumen_bind::methods]
impl DomCollectionIterator {
    #[proto(iter)]
    fn iter(&self, this: This<Value>) -> Value { this.0 }
    #[proto(next)]
    fn next(&self, ctx: &mut Ctx) -> lumen::embed::OpResult<Option<Value>> {
        let length = ctx.get_member(&self.collection, "length").map_err(|_| lumen::embed::OpError::new("TypeError", "collection length failed"))?;
        let Value::Num(length) = length else { return Err(lumen::embed::OpError::new("TypeError", "invalid collection length")); };
        let index = self.index.get();
        if index >= length as usize { return Ok(None); }
        let value = ctx.get_member(&self.collection, &index.to_string()).map_err(|_| lumen::embed::OpError::new("TypeError", "collection read failed"))?;
        self.index.set(index + 1);
        Ok(Some(value))
    }
}

#[lumen_bind::class(name = "DOMTokenList")]
pub struct DomTokenList { pub(crate) realm: Rc<DomRealm>, pub(crate) node: NodeId, pub(crate) owner: Value }

impl DomTokenList {
    fn tokens(&self) -> OpResult<Vec<String>> {
        let session = self.realm.session.borrow();
        let NodeKind::Element { attributes, .. } = session.document().kind(self.node).map_err(dom_error)? else { return Err(OpError::new("TypeError", "classList requires an element")); };
        let mut tokens = Vec::new();
        for token in attributes.iter().find(|(name, _)| name == "class").map_or("", |(_, value)| value).split_ascii_whitespace() { if !tokens.iter().any(|old| old == token) { tokens.push(token.to_owned()); } }
        Ok(tokens)
    }
    fn validate(token: &str) -> OpResult<()> {
        if token.is_empty() { return Err(OpError::new("SyntaxError", "token must be nonempty")); }
        if token.bytes().any(|byte| byte.is_ascii_whitespace()) { return Err(OpError::new("InvalidCharacterError", "token must contain no ASCII whitespace")); }
        Ok(())
    }
    fn write(&self, tokens: &[String]) -> OpResult<()> { self.realm.session.borrow_mut().document_mut().set_attribute(self.node, "class", &tokens.join(" ")).map_err(dom_error) }
}

#[lumen_bind::methods]
impl DomTokenList {
    #[proto(len)]
    fn length(&self) -> OpResult<usize> { Ok(self.tokens()?.len()) }
    #[proto(getitem)]
    fn indexed(&self, index: usize) -> OpResult<Option<String>> { Ok(self.tokens()?.get(index).cloned()) }
    fn item(&self, index: usize) -> OpResult<Option<String>> { self.indexed(index) }
    fn contains(&self, token: &str) -> OpResult<bool> { Self::validate(token)?; Ok(self.tokens()?.iter().any(|old| old == token)) }
    fn add(&self, #[varargs] tokens: Vec<String>) -> OpResult<()> {
        for token in &tokens { Self::validate(token)?; }
        let mut current = self.tokens()?;
        for token in tokens { if !current.contains(&token) { current.push(token); } }
        self.write(&current)
    }
    fn remove(&self, #[varargs] tokens: Vec<String>) -> OpResult<()> {
        for token in &tokens { Self::validate(token)?; }
        let mut current = self.tokens()?;
        current.retain(|token| !tokens.contains(token));
        self.write(&current)
    }
    fn toggle(&self, token: &str, force: Option<bool>) -> OpResult<bool> {
        Self::validate(token)?;
        let mut current = self.tokens()?;
        let found = current.iter().any(|old| old == token);
        let add = force.unwrap_or(!found);
        if add && !found { current.push(token.into()); }
        if !add { current.retain(|old| old != token); }
        self.write(&current)?;
        Ok(add)
    }
    #[getter]
    fn value(&self) -> OpResult<String> { Ok(self.tokens()?.join(" ")) }
    #[setter]
    fn set_value(&self, value: &str) -> OpResult<()> { self.realm.session.borrow_mut().document_mut().set_attribute(self.node, "class", value).map_err(dom_error) }
    #[proto(iter)]
    fn values(&self, this: This<Value>) -> DomCollectionIterator { DomCollectionIterator { collection: this.0, index: Cell::new(0) } }
}
