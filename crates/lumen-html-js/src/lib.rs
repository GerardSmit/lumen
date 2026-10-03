//! DOM objects for one Lumen realm, backed by the shared Rust render session.
use lumen::embed::{Ctx, OpError, OpResult, Value, WeakValue};
use lumen_html::{html, selector, session::RenderSession, Error, Namespace, NodeId, NodeKind};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};
mod events;
use events::{DomEvent, DomEventTarget, TargetData};
mod collections;
use collections::{DomNodeList, DomHtmlCollection, DomTokenList, DomCollectionIterator};
mod style;
use style::DomStyle;
mod reactive;
mod templates;
mod jsx;
mod observers;
mod editing;
mod shadow;
use shadow::{DomShadowRoot, DomSlotElement};

/// Source facades to bundle with an app; their implementation is installed natively.
pub fn module_source(specifier: &str) -> Option<&'static str> {
    match specifier {
        "lumen" => Some("export const {signal,effect,memo,batch,untrack,createRoot,onCleanup,errorBoundary,template,instantiate,nodeAt,bindText,bindAttribute,bindChild,For,Show}=globalThis.__lumen;"),
        "lumen/jsx-runtime" | "lumen/jsx-dev-runtime" => Some("export const {jsx,jsxs,jsxDEV,Fragment}=globalThis.__lumen;"),
        _ => None,
    }
}

#[derive(Debug)]
pub enum InstallError {
    Parse(html::ParseError),
    Global,
}

pub struct DomRealm {
    session: Rc<RefCell<RenderSession>>,
    wrappers: RefCell<HashMap<NodeId, WeakValue>>,
    document_wrapper: RefCell<Option<WeakValue>>,
    detached: RefCell<Vec<NodeId>>,
    sweep_at: Cell<usize>,
    targets: RefCell<HashMap<NodeId, std::rc::Weak<TargetData>>>,
    retained_nodes: RefCell<HashMap<NodeId, usize>>,
    window_target: RefCell<Option<Rc<TargetData>>>,
    window_wrapper: RefCell<Option<WeakValue>>,
    focused: Cell<Option<NodeId>>,
    selections: RefCell<HashMap<NodeId, (usize, usize, String)>>,
}

impl DomRealm {
    pub fn focused_node(&self) -> Option<NodeId> {
        let node = self.focused.get()?;
        let session = self.session.borrow();
        let document = session.document();
        let mut ancestor = node;
        loop {
            if ancestor == document.root() { return Some(node); }
            ancestor = document.shadow_including_parent(ancestor).ok().flatten()?;
        }
    }

    pub fn focus(self: &Rc<Self>, ctx: &mut Ctx, node: Option<NodeId>) -> OpResult<()> {
        if let Some(node) = node {
            let session = self.session.borrow();
            let document = session.document();
            let NodeKind::Element { name, attributes, .. } = document.kind(node).map_err(dom_error)? else { return Err(OpError::new("TypeError", "focus target must be an element")); };
            if attributes.iter().any(|(name, _)| name == "disabled") { return Ok(()); }
            let focusable = matches!(name.as_str(), "input" | "button" | "textarea" | "select" | "summary") || attributes.iter().any(|(attribute, _)| attribute == "tabindex" || attribute == "contenteditable" || (attribute == "href" && matches!(name.as_str(), "a" | "area")));
            if !focusable { return Ok(()); }
            let mut ancestor = node;
            while ancestor != document.root() { let Some(parent) = document.shadow_including_parent(ancestor).map_err(dom_error)? else { return Ok(()); }; ancestor = parent; }
        }
        let old = self.focused_node();
        if old == node { return Ok(()); }
        self.focused.set(None);
        let previous = old.map_or(Value::Null, |old| self.wrap(ctx, old));
        let next_value = node.map_or(Value::Null, |node| self.wrap(ctx, node));
        if let Some(old) = old {
            self.dispatch(ctx, old, "blur", false, false, &[("relatedTarget", next_value.clone())])?;
            self.dispatch(ctx, old, "focusout", true, false, &[("relatedTarget", next_value)])?;
        }
        if self.focused.get().is_some() { return Ok(()); }
        self.focused.set(node);
        if let Some(node) = node {
            if self.focused_node() != Some(node) { self.focused.set(None); return Ok(()); }
            self.dispatch(ctx, node, "focus", false, false, &[("relatedTarget", previous.clone())])?;
            if self.focused_node() == Some(node) { self.dispatch(ctx, node, "focusin", true, false, &[("relatedTarget", previous)])?; }
        }
        Ok(())
    }
    /// Dispatch an input event through the DOM ancestry. `false` means canceled.
    pub fn dispatch(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId, kind: &str, bubbles: bool, cancelable: bool, properties: &[(&str, Value)]) -> lumen::embed::OpResult<bool> {
        self.session.borrow().document().kind(node).map_err(|_| lumen::embed::OpError::new("InvalidStateError", "event target no longer exists"))?;
        let value = self.wrap(ctx, node);
        let options = Value::Obj(ctx.new_object());
        ctx.set_member(&options, "bubbles", Value::Bool(bubbles)).map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        ctx.set_member(&options, "cancelable", Value::Bool(cancelable)).map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        let composed = matches!(kind, "click"|"keydown"|"keyup"|"input"|"beforeinput"|"focus"|"blur"|"focusin"|"focusout") || kind.starts_with("pointer") || kind.starts_with("mouse");
        ctx.set_member(&options, "composed", Value::Bool(composed)).map_err(|_| lumen::embed::OpError::new("TypeError", "event initialization failed"))?;
        let event = DomEvent::new(ctx, kind, Some(options))?;
        if let Some((_, related)) = properties.iter().find(|(name, _)| *name == "relatedTarget") { event.set_related_target(related.clone()); }
        let event = ctx.new_instance(event);
        for (name, property) in properties { if *name != "relatedTarget" { ctx.set_member(&event, name, property.clone()).map_err(|_| lumen::embed::OpError::new("TypeError", "event property initialization failed"))?; } }
        let data = self.targets.borrow().get(&node).and_then(std::rc::Weak::upgrade).ok_or_else(|| lumen::embed::OpError::new("InvalidStateError", "event target no longer exists"))?;
        let allowed = DomEventTarget::from_data(data).dispatch_event(ctx, lumen_bind::This(value), lumen::embed::JsObject::from_value(event).expect("event object"))?;
        if allowed && kind == "keydown" && self.focused_node() == Some(node) { self.edit_control_key(ctx, node, properties)?; }
        Ok(allowed)
    }

    pub fn session_handle(&self) -> Rc<RefCell<RenderSession>> {
        self.session.clone()
    }

    pub fn with_session<R>(&self, work: impl FnOnce(&mut RenderSession) -> R) -> R {
        if !self.detached.borrow().is_empty() {
            self.reap_detached(std::iter::empty());
        }
        work(&mut self.session.borrow_mut())
    }

    pub fn wrapper_count(&self) -> usize {
        self.wrappers.borrow().len()
    }

    fn reap_detached(&self, added: impl IntoIterator<Item = NodeId>) {
        let mut pending = self.detached.borrow_mut();
        pending.extend(added);
        let mut session = self.session.borrow_mut();
        let document = session.document_mut();
        let wrappers = self.wrappers.borrow();
        pending.retain(|&root| {
            if document.kind(root).is_err() || document.parent(root).ok().flatten().is_some() {
                return false;
            }
            let mut cursor = Some(root);
            let mut tree_root = root;
            let mut contents = Vec::new();
            let mut live = false;
            while let Some(id) = cursor {
                if wrappers
                    .get(&id)
                    .is_some_and(|value| value.upgrade().is_some()) || self.retained_nodes.borrow().contains_key(&id)
                {
                    live = true;
                    break;
                }
                if let Ok(Some(content)) = document.template_content(id) { contents.push(content); }
                if let Ok(Some(content)) = document.shadow_root(id) { contents.push(content); }
                cursor = next_descendant(document, tree_root, id).ok().flatten();
                if cursor.is_none() { if let Some(content) = contents.pop() { tree_root = content; cursor = Some(content); } }
            }
            if live {
                true
            } else {
                let _ = document.destroy_subtree(root);
                false
            }
        });
        self.targets.borrow_mut().retain(|id, target| document.kind(*id).is_ok() && target.strong_count() > 0);
        self.selections.borrow_mut().retain(|id, _| document.kind(*id).is_ok());
    }

    fn wrap(self: &Rc<Self>, ctx: &mut Ctx, id: NodeId) -> Value {
        if id == self.session.borrow().document().root() {
            if let Some(value) = self
                .document_wrapper
                .borrow()
                .as_ref()
                .and_then(WeakValue::upgrade)
            {
                return value;
            }
        }
        if let Some(value) = self.wrappers.borrow().get(&id).and_then(WeakValue::upgrade) {
            return value;
        }
        let node = DomNode {
            base: DomEventTarget::node(self, id),
            realm: self.clone(),
            id,
            collections: RefCell::new(HashMap::new()),
        };
        let value = match self.session.borrow().document().kind(id) {
            Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "template" => ctx.cached_instance(id, || DomTemplateElement { base: DomHtmlElement { base: DomElement { base: node } } }),
            Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "iframe" => ctx.cached_instance(id, || DomIFrameElement { base: DomHtmlElement { base: DomElement { base: node } } }),
            Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "input" => ctx.cached_instance(id, || DomInputElement { base: DomHtmlElement { base: DomElement { base: node } } }),
            Ok(NodeKind::Element { namespace: Namespace::Html, name, .. }) if name == "slot" => ctx.cached_instance(id, || DomSlotElement { base: DomHtmlElement { base: DomElement { base: node } } }),
            Ok(NodeKind::Element { namespace: Namespace::Html, .. }) => ctx.cached_instance(id, || DomHtmlElement { base: DomElement { base: node } }),
            Ok(NodeKind::Element { .. }) => ctx.cached_instance(id, || DomElement { base: node }),
            Ok(NodeKind::Text(_)) => ctx.cached_instance(id, || DomText { base: DomCharacterData { base: node } }),
            Ok(NodeKind::Comment(_)) => ctx.cached_instance(id, || DomComment { base: DomCharacterData { base: node } }),
            Ok(NodeKind::DocumentFragment) if self.session.borrow().document().shadow_host(id).ok().flatten().is_some() => ctx.cached_instance(id, || DomShadowRoot { base: DomDocumentFragment { base: node } }),
            Ok(NodeKind::DocumentFragment) => ctx.cached_instance(id, || DomDocumentFragment { base: node }),
            _ => ctx.cached_instance(id, || node),
        };
        let weak = ctx.weak_value(&value).expect("native node is an object");
        let mut wrappers = self.wrappers.borrow_mut();
        wrappers.insert(id, weak);
        if wrappers.len() >= self.sweep_at.get().max(256) {
            wrappers.retain(|_, value| value.upgrade().is_some());
            self.targets.borrow_mut().retain(|_, target| target.strong_count() > 0);
            self.sweep_at.set(wrappers.len().saturating_mul(2).max(256));
        }
        value
    }

    fn wrap_option(self: &Rc<Self>, ctx: &mut Ctx, id: Option<NodeId>) -> Value {
        id.map_or(Value::Null, |id| self.wrap(ctx, id))
    }
}

fn dom_error(error: Error) -> OpError {
    let name = match error { Error::InvalidNode => "NotFoundError", Error::Hierarchy => "HierarchyRequestError", Error::LimitExceeded => "QuotaExceededError", Error::WrongKind | Error::UnsupportedDoctype => "NotSupportedError" };
    OpError::new(name, format!("DOM operation failed: {error:?}"))
}

fn selector_error(error: selector::SelectorError) -> OpError {
    OpError::new("SyntaxError", format!("Invalid selector: {error:?}"))
}

fn named_child(
    document: &lumen_html::Document,
    parent: NodeId,
    wanted: &str,
) -> Result<Option<NodeId>, Error> {
    let mut child = document.first_child(parent)?;
    while let Some(id) = child {
        if matches!(document.kind(id)?, NodeKind::Element { name, .. } if name == wanted) {
            return Ok(Some(id));
        }
        child = document.next_sibling(id)?;
    }
    Ok(None)
}

fn element_by_id(document: &lumen_html::Document, wanted: &str) -> Result<Option<NodeId>, Error> {
    let root = document.root();
    element_by_id_in(document, root, wanted)
}
fn element_by_id_in(document: &lumen_html::Document, root: NodeId, wanted: &str) -> Result<Option<NodeId>, Error> {
    let mut cursor = next_descendant(document, root, root)?;
    while let Some(id) = cursor {
        if let NodeKind::Element { attributes, .. } = document.kind(id)? {
            if attributes
                .iter()
                .any(|(name, value)| name == "id" && value == wanted)
            {
                return Ok(Some(id));
            }
        }
        cursor = next_descendant(document, root, id)?;
    }
    Ok(None)
}

fn next_descendant(
    document: &lumen_html::Document,
    root: NodeId,
    mut node: NodeId,
) -> Result<Option<NodeId>, Error> {
    if let Some(child) = document.first_child(node)? {
        return Ok(Some(child));
    }
    loop {
        if node == root {
            return Ok(None);
        }
        if let Some(sibling) = document.next_sibling(node)? {
            return Ok(Some(sibling));
        }
        node = document.parent(node)?.ok_or(Error::Hierarchy)?;
    }
}

fn text_content(
    document: &lumen_html::Document,
    node: NodeId,
    out: &mut String,
) -> Result<(), Error> {
    let mut cursor = next_descendant(document, node, node)?;
    while let Some(id) = cursor {
        if let NodeKind::Text(text) = document.kind(id)? {
            out.push_str(text);
        }
        cursor = next_descendant(document, node, id)?;
    }
    Ok(())
}

fn children(document: &lumen_html::Document, parent: NodeId) -> Result<Vec<NodeId>, Error> {
    let mut out = Vec::new();
    let mut child = document.first_child(parent)?;
    while let Some(id) = child {
        out.push(id);
        child = document.next_sibling(id)?;
    }
    Ok(out)
}

#[lumen_bind::class(name = "Element", extends = DomNode)]
pub struct DomElement { base: DomNode }
#[lumen_bind::methods]
impl DomElement {
    fn attach_shadow(&self, ctx: &mut Ctx, options: Value) -> OpResult<Value> {
        let mode = match ctx.get_member(&options, "mode").map_err(|_| OpError::new("TypeError", "shadow mode is required"))? {
            Value::Str(value) if value.as_str() == "open" => lumen_html::ShadowMode::Open,
            Value::Str(value) if value.as_str() == "closed" => lumen_html::ShadowMode::Closed,
            _ => return Err(OpError::new("TypeError", "shadow mode must be open or closed")),
        };
        let root = self.base.realm.session.borrow_mut().document_mut().attach_shadow(self.base.id, mode).map_err(|error| if error == Error::Hierarchy { OpError::new("NotSupportedError", "host already has a shadow root") } else { dom_error(error) })?;
        Ok(self.base.realm.wrap(ctx, root))
    }
    #[getter]
    fn shadow_root(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let root = { let session = self.base.realm.session.borrow(); let document = session.document(); document.shadow_root(self.base.id).map_err(dom_error)?.filter(|root| document.shadow_mode(*root).ok().flatten() == Some(lumen_html::ShadowMode::Open)) };
        Ok(self.base.realm.wrap_option(ctx, root))
    }
    #[getter]
    fn slot(&self) -> OpResult<String> { Ok(self.base.get_attribute("slot")?.unwrap_or_default()) }
    #[setter]
    fn set_slot(&self, value: &str) -> OpResult<()> { self.base.set_attribute("slot", value) }
}

#[lumen_bind::class(name = "HTMLElement", extends = DomElement)]
pub struct DomHtmlElement { base: DomElement }
#[lumen_bind::class(name = "HTMLIFrameElement", extends = DomHtmlElement)]
pub struct DomIFrameElement { base: DomHtmlElement }
#[lumen_bind::methods]
impl DomIFrameElement {}
#[lumen_bind::class(name = "HTMLInputElement", extends = DomHtmlElement)]
pub struct DomInputElement { base: DomHtmlElement }
#[lumen_bind::methods]
impl DomInputElement {
    #[getter(name = "type")]
    fn input_type(&self) -> OpResult<String> { Ok(self.base.base.base.get_attribute("type")?.unwrap_or_else(|| "text".into())) }
    #[setter(name = "type")]
    fn set_input_type(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute("type", value) }
    #[getter]
    fn value(&self) -> OpResult<String> { self.base.value() }
    #[setter]
    fn set_value(&self, value: &str) -> OpResult<()> { self.base.set_value(value) }
}
#[lumen_bind::class(name = "HTMLTemplateElement", extends = DomHtmlElement)]
pub struct DomTemplateElement { base: DomHtmlElement }
#[lumen_bind::methods]
impl DomTemplateElement {
    #[getter]
    fn content(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base.base;
        let content = node.realm.session.borrow().document().template_content(node.id).map_err(dom_error)?.unwrap();
        Ok(node.realm.wrap(ctx, content))
    }
}
#[lumen_bind::methods]
impl DomHtmlElement {
    #[getter]
    fn oninput(&self) -> Option<lumen::embed::JsFunction> { self.base.base.base.handler("input") }
    #[setter]
    fn set_oninput(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: Option<lumen::embed::JsFunction>) { self.base.base.base.set_handler(ctx, &this.0, "input", callback); }
    #[getter]
    fn onclick(&self) -> Option<lumen::embed::JsFunction> { self.base.base.base.handler("click") }
    #[setter]
    fn set_onclick(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: Option<lumen::embed::JsFunction>) { self.base.base.base.set_handler(ctx, &this.0, "click", callback); }
    #[getter]
    fn tab_index(&self) -> OpResult<i32> {
        if let Some(value) = self.base.base.get_attribute("tabindex")? { if let Ok(value) = value.parse() { return Ok(value); } }
        let name = self.base.base.local_name()?.unwrap_or_default();
        Ok(if matches!(name.as_str(), "input" | "button" | "textarea" | "select" | "summary") || (matches!(name.as_str(), "a" | "area") && self.base.base.has_attribute("href")?) { 0 } else { -1 })
    }
    #[setter]
    fn set_tab_index(&self, index: i32) -> OpResult<()> { self.base.base.set_attribute("tabindex", &index.to_string()) }
    fn focus(&self, ctx: &mut Ctx) -> OpResult<()> { self.base.base.realm.focus(ctx, Some(self.base.base.id)) }
    fn blur(&self, ctx: &mut Ctx) -> OpResult<()> { if self.base.base.realm.focused_node() == Some(self.base.base.id) { self.base.base.realm.focus(ctx, None)?; } Ok(()) }
    #[getter]
    fn value(&self) -> OpResult<String> {
        if let Some(value) = self.base.base.get_attribute("value")? { return Ok(value); }
        if self.base.base.local_name()?.as_deref() == Some("textarea") { return Ok(self.base.base.text_content()?.unwrap_or_default()); }
        Ok(String::new())
    }
    #[setter]
    fn set_value(&self, value: &str) -> OpResult<()> { self.base.base.set_attribute("value", value)?; let end = value.encode_utf16().count(); self.base.base.realm.set_selection(self.base.base.id, end, end, "none") }
    #[getter]
    fn selection_start(&self) -> OpResult<usize> { Ok(self.base.base.realm.selection(self.base.base.id)?.0) }
    #[setter]
    fn set_selection_start(&self, start: usize) -> OpResult<()> { let (_, end, direction) = self.base.base.realm.selection(self.base.base.id)?; self.base.base.realm.set_selection(self.base.base.id, start, end.max(start), &direction) }
    #[getter]
    fn selection_end(&self) -> OpResult<usize> { Ok(self.base.base.realm.selection(self.base.base.id)?.1) }
    #[setter]
    fn set_selection_end(&self, end: usize) -> OpResult<()> { let (start, _, direction) = self.base.base.realm.selection(self.base.base.id)?; self.base.base.realm.set_selection(self.base.base.id, start, end, &direction) }
    #[getter]
    fn selection_direction(&self) -> OpResult<String> { Ok(self.base.base.realm.selection(self.base.base.id)?.2) }
    #[setter]
    fn set_selection_direction(&self, direction: &str) -> OpResult<()> { let (start, end, _) = self.base.base.realm.selection(self.base.base.id)?; self.base.base.realm.set_selection(self.base.base.id, start, end, direction) }
    fn set_selection_range(&self, start: usize, end: usize, direction: Option<String>) -> OpResult<()> { self.base.base.realm.set_selection(self.base.base.id, start, end, direction.as_deref().unwrap_or("none")) }
    fn select(&self) -> OpResult<()> { let end = self.base.base.realm.control_value(self.base.base.id)?.encode_utf16().count(); self.base.base.realm.set_selection(self.base.base.id, 0, end, "none") }
    #[getter]
    fn checked(&self) -> OpResult<bool> { self.base.base.has_attribute("checked") }
    #[setter]
    fn set_checked(&self, checked: bool) -> OpResult<()> { if checked { self.base.base.set_attribute("checked", "") } else { self.base.base.remove_attribute("checked") } }
    #[getter]
    fn disabled(&self) -> OpResult<bool> { self.base.base.has_attribute("disabled") }
    #[setter]
    fn set_disabled(&self, disabled: bool) -> OpResult<()> { if disabled { self.base.base.set_attribute("disabled", "") } else { self.base.base.remove_attribute("disabled") } }
    #[getter]
    fn read_only(&self) -> OpResult<bool> { self.base.base.has_attribute("readonly") }
    #[setter]
    fn set_read_only(&self, read_only: bool) -> OpResult<()> { if read_only { self.base.base.set_attribute("readonly", "") } else { self.base.base.remove_attribute("readonly") } }
}

#[lumen_bind::class(name = "CharacterData", extends = DomNode)]
pub struct DomCharacterData { base: DomNode }
#[lumen_bind::methods]
impl DomCharacterData {
    #[getter]
    fn data(&self) -> OpResult<String> { Ok(self.base.node_value()?.unwrap_or_default()) }
    #[setter]
    fn set_data(&self, value: &str) -> OpResult<()> { self.base.set_node_value(Some(value)) }
    #[getter]
    fn length(&self) -> OpResult<usize> { Ok(self.data()?.encode_utf16().count()) }
}

#[lumen_bind::class(name = "Text", extends = DomCharacterData)]
pub struct DomText { base: DomCharacterData }
#[lumen_bind::methods]
impl DomText {}

#[lumen_bind::class(name = "Comment", extends = DomCharacterData)]
pub struct DomComment { base: DomCharacterData }
#[lumen_bind::methods]
impl DomComment {}

#[lumen_bind::class(name = "DocumentFragment", extends = DomNode)]
pub struct DomDocumentFragment { base: DomNode }
#[lumen_bind::methods]
impl DomDocumentFragment {}

#[lumen_bind::class(name = "Document", extends = DomNode)]
pub struct DomDocument {
    base: DomNode,
    realm: Rc<DomRealm>,
}

#[lumen_bind::methods]
impl DomDocument {
    #[getter]
    fn oninput(&self) -> Option<lumen::embed::JsFunction> { self.base.base.handler("input") }
    #[setter]
    fn set_oninput(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, callback: Option<lumen::embed::JsFunction>) { self.base.base.set_handler(ctx, &this.0, "input", callback); }
    #[getter]
    fn active_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        if let Some(node) = self.realm.focused_node() { let target = self.realm.session.borrow().document().retarget(node, Some(self.base.id)).map_err(dom_error)?; return Ok(self.realm.wrap(ctx, target)); }
        self.body(ctx)
    }
    #[getter]
    fn document_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = named_child(session.document(), session.document().root(), "html")
            .map_err(dom_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }

    #[getter]
    fn body(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let html = named_child(document, document.root(), "html").map_err(dom_error)?;
        let body = if let Some(html) = html {
            named_child(document, html, "body").map_err(dom_error)?
        } else {
            None
        };
        drop(session);
        Ok(self.realm.wrap_option(ctx, body))
    }

    fn create_element(&self, ctx: &mut Ctx, name: &str) -> OpResult<Value> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(OpError::new("InvalidCharacterError", "invalid element name"));
        }
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: name.to_ascii_lowercase(),
                attributes: Vec::new(),
            })
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    #[method(name = "createElementNS")]
    fn create_element_ns(&self, ctx: &mut Ctx, namespace: &str, name: &str) -> OpResult<Value> {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':'))
        {
            return Err(OpError::new("InvalidCharacterError", "invalid element name"));
        }
        let namespace = match namespace {
            "http://www.w3.org/1999/xhtml" => Namespace::Html,
            "http://www.w3.org/2000/svg" => Namespace::Svg,
            "http://www.w3.org/1998/Math/MathML" => Namespace::MathMl,
            other => Namespace::Other(other.to_owned()),
        };
        let name = if namespace == Namespace::Html {
            name.to_ascii_lowercase()
        } else {
            name.to_owned()
        };
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Element {
                namespace,
                name,
                attributes: Vec::new(),
            })
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    fn create_text_node(&self, ctx: &mut Ctx, text: &str) -> OpResult<Value> {
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Text(text.to_owned()))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    fn create_comment(&self, ctx: &mut Ctx, text: &str) -> OpResult<Value> {
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::Comment(text.to_owned()))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    fn get_element_by_id(&self, ctx: &mut Ctx, id: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let found = element_by_id(session.document(), id).map_err(dom_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, found))
    }

    fn create_document_fragment(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let id = self
            .realm
            .session
            .borrow_mut()
            .document_mut()
            .create(NodeKind::DocumentFragment)
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, id))
    }

    fn query_selector(&self, ctx: &mut Ctx, selector: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::query_selector(session.document(), session.document().root(), selector)
            .map_err(selector_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }
}

#[lumen_bind::class(name = "Node", extends = DomEventTarget)]
pub struct DomNode {
    base: DomEventTarget,
    realm: Rc<DomRealm>,
    id: NodeId,
    collections: RefCell<HashMap<u8, WeakValue>>,
}

#[lumen_bind::methods]
impl DomNode {
    fn get_root_node(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        let composed = match options { Some(value) => matches!(ctx.get_member(&value, "composed").map_err(|_| OpError::new("TypeError", "root options getter failed"))?, Value::Bool(true)), None => false };
        let root = self.realm.session.borrow().document().root_node(self.id, composed).map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, root))
    }
    #[getter]
    fn assigned_slot(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let slot = { let session = self.realm.session.borrow(); let document = session.document(); document.assigned_slot(self.id).map_err(dom_error)?.filter(|slot| document.root_node(*slot, false).ok().and_then(|root| document.shadow_mode(root).ok().flatten()) == Some(lumen_html::ShadowMode::Open)) };
        Ok(self.realm.wrap_option(ctx, slot))
    }
    fn append(&self, #[varargs] nodes: Vec<&DomNode>) -> OpResult<()> {
        if nodes.iter().any(|node| !Rc::ptr_eq(&self.realm, &node.realm)) { return Err(OpError::new("WrongDocumentError", "nodes belong to different documents")); }
        let ids: Vec<NodeId> = nodes.iter().map(|node| node.id).collect();
        self.realm.session.borrow_mut().document_mut().append_many(self.id, &ids).map_err(dom_error)
    }
    fn replace_children(&self, #[varargs] nodes: Vec<&DomNode>) -> OpResult<()> {
        if nodes.iter().any(|node| !Rc::ptr_eq(&self.realm, &node.realm)) { return Err(OpError::new("WrongDocumentError", "nodes belong to different documents")); }
        let ids: Vec<NodeId> = nodes.iter().map(|node| node.id).collect();
        let mut session = self.realm.session.borrow_mut();
        let old = children(session.document(), self.id).map_err(dom_error)?;
        session.document_mut().replace_children_many(self.id, &ids).map_err(dom_error)?;
        drop(session);
        self.realm.reap_detached(old);
        Ok(())
    }
    #[getter(name = "namespaceURI")]
    fn namespace_uri(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element { namespace, .. } => Some(match namespace { Namespace::Html => "http://www.w3.org/1999/xhtml", Namespace::Svg => "http://www.w3.org/2000/svg", Namespace::MathMl => "http://www.w3.org/1998/Math/MathML", Namespace::Other(value) => value }.into()),
            _ => None,
        })
    }
    #[getter]
    fn local_name(&self) -> OpResult<Option<String>> {
        Ok(match self.realm.session.borrow().document().kind(self.id).map_err(dom_error)? { NodeKind::Element { name, .. } => Some(name.clone()), _ => None })
    }
    #[getter]
    fn tag_name(&self) -> OpResult<String> { self.node_name() }
    #[getter]
    fn is_connected(&self) -> OpResult<bool> {
        let session = self.realm.session.borrow();
        let document = session.document();
        let mut current = Some(self.id);
        while let Some(node) = current { if node == document.root() { return Ok(true); } current = document.shadow_including_parent(node).map_err(dom_error)?; }
        Ok(false)
    }
    fn contains(&self, other: Option<&DomNode>) -> OpResult<bool> {
        let Some(other) = other else { return Ok(false); };
        if !Rc::ptr_eq(&self.realm, &other.realm) { return Ok(false); }
        let session = self.realm.session.borrow();
        let mut current = Some(other.id);
        while let Some(node) = current { if node == self.id { return Ok(true); } current = session.document().parent(node).map_err(dom_error)?; }
        Ok(false)
    }
    fn has_attribute(&self, name: &str) -> OpResult<bool> { Ok(self.get_attribute(name)?.is_some()) }
    #[getter]
    fn style(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self.collections.borrow().get(&3).and_then(WeakValue::upgrade) { return value; }
        let value = ctx.new_instance(DomStyle { realm: self.realm.clone(), node: self.id, computed: false, _owner: this.0 });
        self.collections.borrow_mut().insert(3, ctx.weak_value(&value).expect("style object"));
        value
    }

    fn query_selector_all(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>, query: &str) -> OpResult<DomNodeList> {
        let nodes = selector::query_selector_all(self.realm.session.borrow().document(), self.id, query).map_err(selector_error)?;
        Ok(DomNodeList::snapshot(self.realm.clone(), nodes, this.0))
    }

    #[getter]
    fn child_nodes(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self.collections.borrow().get(&0).and_then(WeakValue::upgrade) { return value; }
        let value = ctx.new_instance(DomNodeList::children(self.realm.clone(), self.id, false, this.0));
        self.collections.borrow_mut().insert(0, ctx.weak_value(&value).expect("collection object"));
        value
    }
    #[getter]
    fn children(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self.collections.borrow().get(&1).and_then(WeakValue::upgrade) { return value; }
        let value = ctx.new_instance(DomHtmlCollection { base: DomNodeList::children(self.realm.clone(), self.id, true, this.0) });
        self.collections.borrow_mut().insert(1, ctx.weak_value(&value).expect("collection object"));
        value
    }
    #[getter]
    fn class_list(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
        if let Some(value) = self.collections.borrow().get(&2).and_then(WeakValue::upgrade) { return value; }
        let value = ctx.new_instance(DomTokenList { realm: self.realm.clone(), node: self.id, owner: this.0 });
        self.collections.borrow_mut().insert(2, ctx.weak_value(&value).expect("collection object"));
        value
    }

    #[getter]
    fn owner_document(&self, ctx: &mut Ctx) -> Value {
        let root = self.realm.session.borrow().document().root();
        if self.id == root { Value::Null } else { self.realm.wrap(ctx, root) }
    }

    #[getter]
    fn node_value(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Text(value) | NodeKind::Comment(value) => Some(value.clone()),
            NodeKind::ProcessingInstruction { data, .. } => Some(data.clone()),
            _ => None,
        })
    }

    #[setter]
    fn set_node_value(&self, value: Option<&str>) -> OpResult<()> {
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        match doc.kind(self.id).map_err(dom_error)? {
            NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {
                doc.replace_data(self.id, value.unwrap_or(""))
                    .map_err(dom_error)
            }
            _ => Ok(()),
        }
    }

    #[getter]
    fn node_type(&self) -> OpResult<u8> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element { .. } => 1,
            NodeKind::Text(_) => 3,
            NodeKind::ProcessingInstruction { .. } => 7,
            NodeKind::Comment(_) => 8,
            NodeKind::Document => 9,
            NodeKind::DocumentType(_) => 10,
            NodeKind::DocumentFragment => 11,
        })
    }

    #[getter]
    fn node_name(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        Ok(match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                ..
            } => name.to_ascii_uppercase(),
            NodeKind::Element { name, .. } => name.clone(),
            NodeKind::Text(_) => "#text".into(),
            NodeKind::Comment(_) => "#comment".into(),
            NodeKind::Document => "#document".into(),
            NodeKind::DocumentFragment => "#document-fragment".into(),
            NodeKind::DocumentType(name) => name.clone(),
            NodeKind::ProcessingInstruction { target, .. } => target.clone(),
        })
    }

    #[getter]
    fn parent_node(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let parent = self
            .realm
            .session
            .borrow()
            .document()
            .parent(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, parent))
    }

    #[getter]
    fn first_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = self
            .realm
            .session
            .borrow()
            .document()
            .first_child(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn last_child(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let child = self
            .realm
            .session
            .borrow()
            .document()
            .last_child(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, child))
    }

    #[getter]
    fn previous_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = self
            .realm
            .session
            .borrow()
            .document()
            .previous_sibling(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    #[getter]
    fn next_sibling(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let sibling = self
            .realm
            .session
            .borrow()
            .document()
            .next_sibling(self.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap_option(ctx, sibling))
    }

    fn append_child(&self, ctx: &mut Ctx, child: &DomNode) -> OpResult<Value> {
        if !Rc::ptr_eq(&self.realm, &child.realm) {
            return Err(OpError::new("InvalidStateError", 
                "nodes belong to different documents",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .append(self.id, child.id)
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, child.id))
    }

    fn insert_before(
        &self,
        ctx: &mut Ctx,
        child: &DomNode,
        before: Option<&DomNode>,
    ) -> OpResult<Value> {
        if !Rc::ptr_eq(&self.realm, &child.realm)
            || before.is_some_and(|node| !Rc::ptr_eq(&self.realm, &node.realm))
        {
            return Err(OpError::new("InvalidStateError", 
                "nodes belong to different documents",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .insert_before(self.id, child.id, before.map(|node| node.id))
            .map_err(dom_error)?;
        Ok(self.realm.wrap(ctx, child.id))
    }

    fn remove_child(&self, ctx: &mut Ctx, child: &DomNode) -> OpResult<Value> {
        if !Rc::ptr_eq(&self.realm, &child.realm)
            || self
                .realm
                .session
                .borrow()
                .document()
                .parent(child.id)
                .map_err(dom_error)?
                != Some(self.id)
        {
            return Err(OpError::new("NotFoundError", "node is not a child"));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(child.id)
            .map_err(dom_error)?;
        let value = self.realm.wrap(ctx, child.id);
        self.realm.reap_detached([child.id]);
        Ok(value)
    }

    fn remove(&self) -> OpResult<()> {
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove(self.id)
            .map_err(dom_error)?;
        self.realm.reap_detached([self.id]);
        Ok(())
    }

    fn clone_node(&self, ctx: &mut Ctx, deep: Option<bool>) -> OpResult<Value> {
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let id = if deep.unwrap_or(false) {
            doc.clone_subtree(self.id)
        } else {
            let kind = doc.kind(self.id).map_err(dom_error)?.clone();
            doc.create(kind)
        }
        .map_err(dom_error)?;
        drop(session);
        Ok(self.realm.wrap(ctx, id))
    }

    fn set_attribute(&self, name: &str, value: &str) -> OpResult<()> {
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_attribute(self.id, name, value)
            .map_err(dom_error)
    }

    fn get_attribute(&self, name: &str) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        let NodeKind::Element { attributes, .. } =
            session.document().kind(self.id).map_err(dom_error)?
        else {
            return Err(OpError::new("TypeError", "attributes require an element"));
        };
        Ok(attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone()))
    }

    #[getter]
    fn id(&self) -> OpResult<String> {
        Ok(self.get_attribute("id")?.unwrap_or_default())
    }

    #[setter]
    fn set_id(&self, value: &str) -> OpResult<()> {
        self.set_attribute("id", value)
    }

    #[getter]
    fn class_name(&self) -> OpResult<String> {
        Ok(self.get_attribute("class")?.unwrap_or_default())
    }

    #[setter]
    fn set_class_name(&self, value: &str) -> OpResult<()> {
        self.set_attribute("class", value)
    }

    fn remove_attribute(&self, name: &str) -> OpResult<()> {
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .remove_attribute(self.id, name)
            .map_err(dom_error)
    }

    #[getter(name = "textContent")]
    fn text_content(&self) -> OpResult<Option<String>> {
        let session = self.realm.session.borrow();
        match session.document().kind(self.id).map_err(dom_error)? {
            NodeKind::Text(text) | NodeKind::Comment(text) => return Ok(Some(text.clone())),
            NodeKind::ProcessingInstruction { data, .. } => return Ok(Some(data.clone())),
            NodeKind::Document | NodeKind::DocumentType(_) => return Ok(None),
            _ => {},
        }
        let mut out = String::new();
        text_content(session.document(), self.id, &mut out).map_err(dom_error)?;
        Ok(Some(out))
    }

    #[setter(name = "textContent")]
    fn set_text_content(&self, value: &str) -> OpResult<()> {
        let mut session = self.realm.session.borrow_mut();
        let doc = session.document_mut();
        let old = if matches!(
            doc.kind(self.id).map_err(dom_error)?,
            NodeKind::Element { .. } | NodeKind::DocumentFragment
        ) {
            children(doc, self.id).map_err(dom_error)?
        } else {
            Vec::new()
        };
        let result = match doc.kind(self.id).map_err(dom_error)? {
            NodeKind::Text(_) | NodeKind::Comment(_) | NodeKind::ProcessingInstruction { .. } => {
                doc.replace_data(self.id, value).map_err(dom_error)
            }
            NodeKind::Element { .. } | NodeKind::DocumentFragment => {
                let replacement = if value.is_empty() {
                    doc.create(NodeKind::DocumentFragment)
                } else {
                    doc.create(NodeKind::Text(value.to_owned()))
                }
                .map_err(dom_error)?;
                let result = doc
                    .replace_children(self.id, replacement)
                    .map_err(dom_error);
                if value.is_empty() {
                    doc.destroy_subtree(replacement).map_err(dom_error)?;
                }
                result
            }
            _ => Ok(()),
        };
        drop(session);
        if result.is_ok() {
            self.realm.reap_detached(old);
        }
        result
    }

    #[getter(name = "innerHTML")]
    fn inner_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        html::inner_html(session.document(), self.id).map_err(dom_error)
    }

    #[setter(name = "innerHTML")]
    fn set_inner_html(&self, value: &str) -> OpResult<()> {
        let mut session = self.realm.session.borrow_mut();
        let document = session.document_mut();
        let context = document.shadow_host(self.id).map_err(dom_error)?.unwrap_or(self.id);
        let fragment = html::parse_fragment_in(document, context, value).map_err(|error| {
            OpError::new("InvalidStateError", format!(
                "HTML parse error at {}: {}",
                error.offset, error.message
            ))
        })?;
        let target = document.template_content(self.id).map_err(dom_error)?.unwrap_or(self.id);
        let old = children(document, target).map_err(dom_error)?;
        let result = document
            .replace_children(target, fragment)
            .map_err(dom_error);
        document.destroy_subtree(fragment).map_err(dom_error)?;
        drop(session);
        if result.is_ok() {
            self.realm.reap_detached(old);
        }
        result
    }

    #[getter(name = "outerHTML")]
    fn outer_html(&self) -> OpResult<String> {
        let session = self.realm.session.borrow();
        html::outer_html(session.document(), self.id).map_err(dom_error)
    }

    fn query_selector(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id =
            selector::query_selector(session.document(), self.id, query).map_err(selector_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }

    fn matches(&self, query: &str) -> OpResult<bool> {
        let session = self.realm.session.borrow();
        selector::matches(session.document(), self.id, query).map_err(selector_error)
    }

    fn closest(&self, ctx: &mut Ctx, query: &str) -> OpResult<Value> {
        let session = self.realm.session.borrow();
        let id = selector::closest(session.document(), self.id, query).map_err(selector_error)?;
        drop(session);
        Ok(self.realm.wrap_option(ctx, id))
    }
}

pub fn install(
    ctx: &mut Ctx,
    source: &str,
    max_nodes: usize,
) -> Result<Rc<DomRealm>, InstallError> {
    let document = html::parse(source, max_nodes).map_err(InstallError::Parse)?;
    let realm = Rc::new(DomRealm {
        session: Rc::new(RefCell::new(RenderSession::new(document))),
        wrappers: RefCell::new(HashMap::new()),
        document_wrapper: RefCell::new(None),
        detached: RefCell::new(Vec::new()),
        sweep_at: Cell::new(256),
        targets: RefCell::new(HashMap::new()),
        retained_nodes: RefCell::new(HashMap::new()),
        window_target: RefCell::new(None),
        window_wrapper: RefCell::new(None),
        focused: Cell::new(None),
    selections: RefCell::new(HashMap::new()),
    });
    let global = ctx.global_object();
    // Materialize a host's lazy web-event unit before replacing its DOM classes.
    ctx.get_member(&global, "EventTarget").map_err(|_| InstallError::Global)?;
    let window_target = DomEventTarget::window(&realm);
    *realm.window_target.borrow_mut() = Some(window_target.data_handle());
    *realm.window_wrapper.borrow_mut() = ctx.weak_value(&global);
    ctx.attach_instance(&global, window_target).map_err(|_| InstallError::Global)?;
    let node = ctx.class_constructor::<DomNode>();
    let document_class = ctx.class_constructor::<DomDocument>();
    let document = ctx.new_instance(DomDocument {
        base: DomNode { base: DomEventTarget::node(&realm, realm.session.borrow().document().root()), realm: realm.clone(), id: realm.session.borrow().document().root(), collections: RefCell::new(HashMap::new()) },
        realm: realm.clone(),
    });
    *realm.document_wrapper.borrow_mut() = ctx.weak_value(&document);
    for (name, ctor) in [
        ("EventTarget", ctx.class_constructor::<DomEventTarget>()),
        ("Event", ctx.class_constructor::<DomEvent>()),
        ("NodeList", ctx.class_constructor::<DomNodeList>()),
        ("HTMLCollection", ctx.class_constructor::<DomHtmlCollection>()),
        ("DOMTokenList", ctx.class_constructor::<DomTokenList>()),
        ("CSSStyleDeclaration", ctx.class_constructor::<DomStyle>()),
        ("Element", ctx.class_constructor::<DomElement>()),
        ("HTMLElement", ctx.class_constructor::<DomHtmlElement>()),
        ("HTMLIFrameElement", ctx.class_constructor::<DomIFrameElement>()),
        ("HTMLInputElement", ctx.class_constructor::<DomInputElement>()),
        ("HTMLSlotElement", ctx.class_constructor::<DomSlotElement>()),
        ("ShadowRoot", ctx.class_constructor::<DomShadowRoot>()),
        ("HTMLTemplateElement", ctx.class_constructor::<DomTemplateElement>()),
        ("CharacterData", ctx.class_constructor::<DomCharacterData>()),
        ("Text", ctx.class_constructor::<DomText>()),
        ("Comment", ctx.class_constructor::<DomComment>()),
        ("DocumentFragment", ctx.class_constructor::<DomDocumentFragment>()),
    ] { ctx.set_member(&global, name, ctor).map_err(|_| InstallError::Global)?; }
    ctx.class_constructor::<DomCollectionIterator>();
    let computed_style = ctx.bound_function(&lumen_bind::FnItem::of::<style::get_computed_style::Op>());
    ctx.set_member(&global, "getComputedStyle", computed_style).map_err(|_| InstallError::Global)?;
    let runtime = reactive::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "__lumen", runtime).map_err(|_| InstallError::Global)?;
    observers::install(ctx, &realm).map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "Node", node)
        .map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "Document", document_class)
        .map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "document", document)
        .map_err(|_| InstallError::Global)?;
    ctx.set_member(&global, "window", global.clone())
        .map_err(|_| InstallError::Global)?;
    Ok(realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    use lumen_html_image::render_with_font;
    use lumen_html_text::{FontFace, DEFAULT_FONT_BYTES};
    use std::sync::Arc;

    fn script(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("valid script") {
            Ok(value) => value,
            Err(error) => {
                let message = engine.ctx().get_member(&error, "message").ok().and_then(|value| if let Value::Str(message) = value { Some(message.to_string()) } else { None }).unwrap_or_else(|| "script threw".into());
                panic!("{message}");
            }
        }
    }

    #[test]
    fn native_dom_classes_inherit_and_dispatch_base_methods() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div>x<!--y--></div>", 64).unwrap();
        let value = engine.eval_value("const div = document.querySelector('div'); const text = div.firstChild; const comment = div.lastChild; div instanceof HTMLElement && div instanceof Element && div instanceof Node && div instanceof EventTarget && text instanceof Text && text instanceof CharacterData && text instanceof Node && comment instanceof Comment && document instanceof Document && document instanceof Node && document.nodeType === 9 && text.data === 'x' && Text.prototype instanceof CharacterData && Object.getPrototypeOf(HTMLElement) === Element && document.createDocumentFragment() instanceof DocumentFragment").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn collections_are_live_indexed_iterable_and_validate_all_tokens() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><b>A</b>text<i>B</i></main>", 64).unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const nodes = main.childNodes; const elements = main.children; const live = nodes instanceof NodeList && elements instanceof HTMLCollection && nodes.length === 3 && elements.length === 2 && nodes[0] === main.firstChild && elements[1] === main.lastChild && ('1' in nodes) && !('3' in nodes) && Object.keys(nodes).join(',') === '0,1,2' && Object.getOwnPropertyDescriptor(nodes, '0').value === main.firstChild && [...nodes].length === 3; main.appendChild(document.createElement('p')); main.classList.add('a', 'b'); let rejected = false; try { main.classList.add('ok', 'bad token'); } catch(e) { rejected = true; } live && nodes === main.childNodes && nodes.length === 4 && elements.length === 3 && rejected && !main.classList.contains('ok') && main.classList[1] === 'b' && [...main.classList].join(',') === 'a,b' && nodes[99] === undefined && nodes.item(99) === null").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn events_capture_bubble_once_passive_and_survive_gc() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><button></button></main>", 64).unwrap();
        engine.eval_value("var order = []; var button = document.querySelector('button'); var main = button.parentNode; main.addEventListener('click', e => order.push('capture:' + e.eventPhase), true); button.addEventListener('click', e => { order.push('target:' + e.eventPhase); e.preventDefault(); }, { once: true }); main.addEventListener('click', e => { order.push('bubble:' + e.eventPhase); e.preventDefault(); }, { passive: true });").unwrap().ok().unwrap();
        engine.collect_garbage();
        let value = engine.eval_value("const e = new Event('click', {bubbles: true, cancelable: true}); const first = button.dispatchEvent(e); const target = e.target === button && e.currentTarget === null && e.eventPhase === 0; const second = button.dispatchEvent(new Event('click', {bubbles: true, cancelable: true})); !first && second && target && order.join(',') === 'capture:1,target:2,bubble:3,capture:1,bubble:3'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn expando_wrappers_remain_identical_across_collection() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        engine.eval_value("document.querySelector('div').saved = 42").unwrap().ok().unwrap();
        engine.collect_garbage();
        let value = engine.eval_value("document.querySelector('div').saved === 42").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn host_dispatch_returns_an_error_for_destroyed_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div></div>", 64).unwrap();
        let node = realm.with_session(|session| {
            let node = selector::query_selector(session.document(), session.document().root(), "div").unwrap().unwrap();
            session.document_mut().remove(node).unwrap();
            session.document_mut().destroy_subtree(node).unwrap();
            node
        });
        assert!(realm.dispatch(engine.ctx(), node, "click", true, true, &[]).is_err());
    }

    #[test]
    fn style_declarations_and_static_queries_share_the_arena() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><b style='color:red'>old</b></main>", 64).unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const list = main.querySelectorAll('b'); const b = list[0]; const style = b.style; style.width = '12px'; style.setProperty('height','8px','important'); const computed = getComputedStyle(b); main.innerHTML = ''; style instanceof CSSStyleDeclaration && style === b.style && style.width === '12px' && style.getPropertyPriority('height') === 'important' && computed.width === '12px' && computed.color === 'rgb(255, 0, 0)' && list.length === 1 && list[0] === b && b.textContent === 'old'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
        assert!(realm.wrapper_count() < 10);
    }

    #[test]
    fn variadic_mutations_validate_before_moving_nodes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><i></i></main>", 64).unwrap();
        let value = engine.eval_value("const main = document.querySelector('main'); const old = main.firstChild; const a = document.createElement('a'); const b = document.createElement('b'); let types = false; let hierarchy = false; try { main.append(a, {}); } catch(e) { types = true; } try { main.append(b, main); } catch(e) { hierarchy = true; } const unchanged = a.parentNode === null && b.parentNode === null && main.firstChild === old; main.replaceChildren(a, b); types && hierarchy && unchanged && main.firstChild === a && main.lastChild === b && old.parentNode === null && main.childNodes.length === 2").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn events_reach_window_and_static_lists_retain_untouched_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><b>retained</b></main>", 64).unwrap();
        assert_eq!(realm.wrapper_count(), 0);
        let value = engine.eval_value("const main = document.querySelector('main'); const list = document.querySelectorAll('b'); let reached = 0; window.addEventListener('ping', () => reached++); main.dispatchEvent(new Event('ping', {bubbles: true})); main.innerHTML = ''; window instanceof EventTarget && reached === 1 && list.length === 1 && list[0].textContent === 'retained' && !list[0].isConnected && document.ownerDocument === null && document.textContent === null").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn reactive_jobs_batch_dependencies_memos_and_cleanup() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(&mut engine, "var values = []; var cleaned = 0; var dispose; var setA; var setB; var choose; var setChoose; var total; __lumen.createRoot(d => { dispose = d; const a = __lumen.signal(1); const b = __lumen.signal(10); const c = __lumen.signal(true); setA = a[1]; setB = b[1]; choose = c[0]; setChoose = c[1]; total = __lumen.memo(() => a[0]() * 2); __lumen.effect(() => { __lumen.onCleanup(() => cleaned++); values.push(choose() ? a[0]() : b[0]()); }); });");
        engine.ctx().drain_microtasks_for_host();
        let value = engine.eval_value("__lumen.batch(() => { setA(2); setA(3); }); total() === 6").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        engine.eval_value("setChoose(false)").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        engine.eval_value("setA(4); setB(11)").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        let value = engine.eval_value("dispose(); setB(12); values.join(',') === '1,3,10,11' && cleaned === 4").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn reactive_errors_reach_their_owner_boundary() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        script(&mut engine, "var caught = ''; __lumen.errorBoundary(() => { __lumen.effect(() => { throw 'boom'; }); }, e => { caught = e; });");
        engine.ctx().drain_microtasks_for_host();
        let value = engine.eval_value("caught === 'boom'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn native_templates_bind_only_touched_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app></div>", 128).unwrap();
        engine.eval_value("var template = __lumen.template('<section><span> </span><b>static</b></section>');").unwrap().ok().unwrap();
        assert_eq!(realm.wrapper_count(), 0);
        script(&mut engine, "var state = __lumen.signal('one'); var tree = __lumen.instantiate(template); var slot = __lumen.nodeAt(tree, [0,0]); __lumen.bindText(slot, state[0]); __lumen.bindAttribute(tree, 'class', () => state[0]()); document.getElementById('app').appendChild(tree);");
        assert_eq!(realm.wrapper_count(), 3);
        engine.eval_value("state[1]('two')").unwrap().ok().unwrap();
        engine.ctx().drain_microtasks_for_host();
        let value = engine.eval_value("tree.textContent === 'twostatic' && tree.className === 'two' && slot === __lumen.nodeAt(tree,[0,0])").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn keyed_regions_preserve_rows_and_dispose_removed_owners() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(&mut engine, "var rows = __lumen.signal(['a','b']); var renders = 0; var cleanedRows = 0; var indexes = []; var main = document.querySelector('main'); main.appendChild(__lumen.For({each:rows[0],children:(item,index) => { renders++; indexes.push(index); __lumen.onCleanup(() => cleanedRows++); const node = document.createElement('b'); node.textContent = item; return node; }})); var firstRow = main.firstChild; rows[1](['b','a']);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "main.textContent === 'ba' && main.childNodes[1] === firstRow && renders === 2 && indexes[0]() === 1 && indexes[1]() === 0"), Value::Bool(true)));
        script(&mut engine, "rows[1](['a']);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "main.textContent === 'a' && main.firstChild === firstRow && cleanedRows === 1"), Value::Bool(true)));
    }

    #[test]
    fn native_key_defaults_edit_inputs_and_respect_cancellation_and_limits() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input maxlength=2><textarea>a</textarea>", 128).unwrap();
        script(&mut engine, "var input = document.querySelector('input'); var textarea = document.querySelector('textarea'); var edits = 0; input.addEventListener('input',() => edits++); input.addEventListener('beforeinput',e => { if(e.data === 'q') e.preventDefault(); }); input.focus();");
        let input = realm.focused_node().unwrap();
        for key in ["x", "y", "z", "Backspace", "q"] { realm.dispatch(engine.ctx(), input, "keydown", true, true, &[("key", Value::str(key))]).unwrap(); }
        assert!(matches!(script(&mut engine, "input.value === 'x' && edits === 3"), Value::Bool(true)));
        script(&mut engine, "input.readOnly = true;");
        realm.dispatch(engine.ctx(), input, "keydown", true, true, &[("key", Value::str("a"))]).unwrap();
        assert!(matches!(script(&mut engine, "input.value === 'x' && edits === 3"), Value::Bool(true)));
        script(&mut engine, "textarea.focus();");
        let textarea = realm.focused_node().unwrap();
        realm.dispatch(engine.ctx(), textarea, "keydown", true, true, &[("key", Value::str("Enter"))]).unwrap();
        realm.dispatch(engine.ctx(), textarea, "keydown", true, true, &[("key", Value::str("c")), ("ctrlKey", Value::Bool(true))]).unwrap();
        assert!(matches!(script(&mut engine, "textarea.value === 'a\\n'"), Value::Bool(true)));
    }

    #[test]
    fn mutation_observers_deliver_filtered_old_values_and_detached_subtrees() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><b>old</b></main>", 128).unwrap();
        script(&mut engine, "var main = document.querySelector('main'); var child = main.firstChild; var observed = []; var observer = new MutationObserver(records => observed.push(...records)); observer.observe(main,{subtree:true,attributes:true,attributeOldValue:true,attributeFilter:['title'],characterData:true,characterDataOldValue:true,childList:true}); child.setAttribute('title','one'); child.setAttribute('title','two'); child.setAttribute('class','ignored'); child.firstChild.data = 'new'; main.removeChild(child); child.setAttribute('title','detached'); var observerOrder = []; new MutationObserver(() => observerOrder.push('observer')).observe(main,{attributes:true}); main.setAttribute('title','schedule'); Promise.resolve().then(() => observerOrder.push('promise'));");
        engine.collect_garbage();
        realm.with_session(|session| { session.document_mut().clear_mutations(); });
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "observed.length === 6 && observed[0].oldValue === null && observed[1].oldValue === 'one' && observed[2].oldValue === 'old' && observed[3].removedNodes[0] === child && observed[4].oldValue === 'two' && observerOrder.join(',') === 'observer,promise'"), Value::Bool(true)));
        script(&mut engine, "child.setAttribute('title','after-delivery'); observer.disconnect(); main.setAttribute('title','disconnected');");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "observed.length === 6 && observer.takeRecords().length === 0"), Value::Bool(true)));
    }

    #[test]
    fn mutation_observer_take_records_does_not_call_callback() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(script(&mut engine, "var called = 0; var observer = new MutationObserver(() => called++); var main = document.querySelector('main'); observer.observe(main,{childList:true}); main.appendChild(document.createElement('b')); var records = observer.takeRecords(); records.length === 1 && records[0].addedNodes[0] === main.firstChild"), Value::Bool(true)));
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "called === 0"), Value::Bool(true)));
    }

    #[test]
    fn focus_input_properties_and_keyboard_events_follow_active_element() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><input id=first><button id=second></button></main>", 128).unwrap();
        assert!(matches!(script(&mut engine, "var first = document.getElementById('first'); var second = document.getElementById('second'); var focusOrder = []; document.querySelector('main').addEventListener('focusin', e => focusOrder.push(e.target.id)); first.value = 'typed'; first.checked = true; first.focus(); second.disabled = true; second.focus(); document.activeElement === first && first.value === 'typed' && first.checked && focusOrder.join(',') === 'first'"), Value::Bool(true)));
        let focused = realm.focused_node().unwrap();
        script(&mut engine, "first.addEventListener('keydown', e => { first.value += e.key; e.preventDefault(); });");
        assert!(!realm.dispatch(engine.ctx(), focused, "keydown", true, true, &[("key", Value::str("x"))]).unwrap());
        assert!(matches!(script(&mut engine, "second.disabled = false; second.focus(); second.blur(); first.value === 'typedx' && document.activeElement === document.body && focusOrder.join(',') === 'first,second'"), Value::Bool(true)));
        assert_eq!(realm.focused_node(), None);
    }

    #[test]
    fn web_targets_capture_bubble_and_preserve_abort_signals() {
        let mut engine = Engine::new();
        script(&mut engine, "globalThis.performance = {now:() => 0};");
        script(&mut engine, include_str!("../../lumen-web/src/js/events.js"));
        let value = script(&mut engine, "const root = new EventTarget(); const leaf = new EventTarget(); leaf.parentNode = root; const order = []; root.addEventListener('ping', e => order.push('capture'+e.eventPhase), true); leaf.addEventListener('ping', e => { order.push('target'+e.eventPhase); e.preventDefault(); }, {passive:true}); root.addEventListener('ping', e => order.push('bubble'+e.eventPhase)); const event = new Event('ping',{bubbles:true,cancelable:true}); const dispatched = leaf.dispatchEvent(event); const controller = new AbortController(); let aborts = 0; controller.signal.addEventListener('abort',() => aborts++); controller.abort(); dispatched && !event.defaultPrevented && event.currentTarget === null && event.eventPhase === 0 && order.join(',') === 'capture1,target2,bubble3' && aborts === 1 && controller.signal.aborted");
        assert!(matches!(value, Value::Bool(true)));
        install(engine.ctx(), "<main></main>", 64).unwrap();
        assert!(matches!(script(&mut engine, "let domError; try { document.querySelector('main').appendChild(document); } catch (error) { domError = error; } domError instanceof DOMException && domError.name === 'HierarchyRequestError'"), Value::Bool(true)));
    }

    #[test]
    fn child_slots_and_show_replace_owned_regions() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        script(&mut engine, "var main = document.querySelector('main'); var marker = document.createComment(''); main.appendChild(marker); var child = __lumen.signal('first'); __lumen.bindChild(marker,child[0]); var visible = __lumen.signal(false); main.appendChild(__lumen.Show({when:visible[0],children:() => 'shown',fallback:'hidden'})); var replacement = document.createElement('b'); replacement.textContent = 'node'; child[1]([replacement,'tail']); visible[1](true);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "main.textContent === 'nodetailshown' && main.firstChild === replacement"), Value::Bool(true)));
        script(&mut engine, "child[1](null); visible[1](false);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "main.textContent === 'hidden' && replacement.parentNode === null"), Value::Bool(true)));
    }

    #[test]
    fn compiled_jsx_slots_update_native_dom() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state = __lumen.signal('one'); globalThis.updateSlot = state[1]; globalThis.compiledTree = <div title={state[0]()}><span>{state[0]()}</span><b>static</b></div>; document.querySelector('main').appendChild(compiledTree);", "app.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        script(&mut engine, "updateSlot('two');");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "compiledTree.getAttribute('title') === 'two' && compiledTree.textContent === 'twostatic'"), Value::Bool(true)));
        let mut static_engine = Engine::new();
        let static_realm = install(static_engine.ctx(), "<main><div title=two><span>two</span><b>static</b></div></main>", 128).unwrap();
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let render = |realm: &Rc<DomRealm>| realm.with_session(|session| render_with_font(session.display_list(160, 80, &font).unwrap(), 160, 80, 1.0, false, &font).unwrap().pixels);
        assert_eq!(render(&realm), render(&static_realm));
    }

    #[test]
    fn compiled_jsx_object_styles_preserve_units_and_replace_removed_properties() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state=__lumen.signal({width:0,height:10,opacity:0,zIndex:2,'--Size':0}); globalThis.updateStyles=state[1]; globalThis.styled=<div style={state[0]()}/>; document.querySelector('main').appendChild(styled);", "style.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        assert!(matches!(script(&mut engine, "styled.style.getPropertyValue('width')==='0px' && styled.style.getPropertyValue('height')==='10px' && styled.style.getPropertyValue('opacity')==='0' && styled.style.getPropertyValue('z-index')==='2' && styled.style.getPropertyValue('--Size')==='0'"), Value::Bool(true)));
        script(&mut engine, "updateStyles({width:4,fontSize:12,opacity:1,'--Size':2});");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "styled.style.getPropertyValue('width')==='4px' && styled.style.getPropertyValue('font-size')==='12px' && styled.style.getPropertyValue('opacity')==='1' && styled.style.getPropertyValue('--Size')==='2' && styled.style.getPropertyValue('height')==='' && styled.style.getPropertyValue('z-index')===''"), Value::Bool(true)));
    }

    #[test]
    fn compiled_mixed_child_slots_keep_static_siblings() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_module_jsx("/** @jsxImportSource lumen */ const state = __lumen.signal(2); globalThis.updateMixed = state[1]; globalThis.mixed = <div>before {state[0]()}<span>{state[0]()}</span>{[state[0](),3]}</div>; document.querySelector('main').appendChild(mixed);", "mixed.tsx", true, |_, _| None).unwrap();
        assert!(!matches!(result, lumen::Completion::Throw { .. }));
        script(&mut engine, "var staticSpan = mixed.querySelector('span'); updateMixed(4);");
        engine.ctx().drain_microtasks_for_host();
        assert!(matches!(script(&mut engine, "mixed.textContent === 'before 4443' && mixed.querySelector('span') === staticSpan"), Value::Bool(true)));
    }

    #[test]
    fn jsx_runtime_creates_dom_for_components_fragments_and_events() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let value = engine.eval_value("const {jsx,jsxs,Fragment} = __lumen; let clicked = 0; function Button(props) { return jsx('button', {className:'ok', onClick:() => clicked++, children:props.label}); } const tree = jsxs('section', {style:{width:20,height:10,backgroundColor:'red'}, children:[jsx(Button,{label:'press'}), jsx(Fragment,{children:['a','b']})]}); document.querySelector('main').appendChild(tree); tree.firstChild.dispatchEvent(new Event('click')); clicked === 1 && tree.textContent === 'pressab' && tree.firstChild instanceof HTMLElement && tree.style.width === '20px'").unwrap().ok().unwrap();
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn js_mutates_the_render_document_with_stable_wrappers() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<!doctype html><div id=app>old</div>", 128).unwrap();
        let result = engine.eval_value("const app = document.querySelector('#app'); const same = app === document.body.firstChild; app.innerHTML = '<span>new</span>'; const span = app.firstChild; span.setAttribute('class', 'ok'); same && app === document.querySelector('#app') && span === app.querySelector('span') && span.matches('.ok') && document.documentElement.nodeName === 'HTML' && document.body.parentNode === document.documentElement && document.documentElement.parentNode === document && app.innerHTML === '<span class=\"ok\">new</span>'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let app =
                selector::query_selector(session.document(), session.document().root(), "#app")
                    .unwrap()
                    .unwrap();
            assert_eq!(
                html::inner_html(session.document(), app).unwrap(),
                "<span class=\"ok\">new</span>"
            );
        });
    }

    #[test]
    fn node_moves_clone_and_text_content_follow_dom_identity() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<main><p>A</p><p>B</p></main>", 128).unwrap();
        let result = engine.eval_value("const main = document.querySelector('main'); const first = main.firstChild; const second = main.lastChild; const copy = first.cloneNode(true); const cloned = copy !== first && copy.textContent === 'A'; const moved = main.insertBefore(second, first) === second && main.firstChild === second && first.previousSibling === second; const removed = main.removeChild(second) === second && second.parentNode === null; main.appendChild(second); first.textContent = 'new'; cloned && moved && removed && main.textContent === 'newB' && first.firstChild.textContent === 'new'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        realm.with_session(|session| {
            let main =
                selector::query_selector(session.document(), session.document().root(), "main")
                    .unwrap()
                    .unwrap();
            let mut text = String::new();
            text_content(session.document(), main, &mut text).unwrap();
            assert_eq!(text, "newB");
        });
    }

    #[test]
    fn template_content_identity_and_inner_html() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<template><b>A</b><template><i>B</i></template></template>", 128).unwrap();
        let result = engine.eval_value("const t=document.querySelector('template'); const c=t.content; const copy=t.cloneNode(true); const ok=t instanceof HTMLTemplateElement && c instanceof DocumentFragment && c===t.content && c.parentNode===null && t.childNodes.length===0 && t.textContent==='' && document.querySelector('b')===null && c.querySelector('b').textContent==='A' && copy.content!==c && copy.innerHTML===t.innerHTML; t.innerHTML='<i>C</i>'; ok && t.content===c && c.firstChild.textContent==='C' && t.childNodes.length===0").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn js_style_mutation_changes_rendered_pixels() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<div style='width:20px;height:20px;background:red'></div>",
            64,
        )
        .unwrap();
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let before = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        engine.eval_value("document.querySelector('div').setAttribute('style', 'width:20px;height:20px;background:blue')").unwrap().ok().unwrap();
        let after = realm.with_session(|session| {
            render_with_font(
                session.display_list(32, 32, &font).unwrap(),
                32,
                32,
                1.0,
                false,
                &font,
            )
            .unwrap()
        });
        assert_ne!(before.pixels, after.pixels);
        assert_eq!(after.pixels.len(), 32 * 32 * 4);
    }

    #[test]
    fn replacing_stylesheet_rebuilds_cached_css() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<style>div{width:20px;height:20px;background:red}</style><div></div>", 128).unwrap();
        realm.with_session(|session| {
            let doc = session.document();
            let head = selector::query_selector(doc, doc.root(), "head").unwrap().unwrap();
            let style = selector::query_selector(doc, doc.root(), "style").unwrap().unwrap();
            assert_eq!(doc.parent(style).unwrap(), Some(head));
        });
        let font = FontFace::new(Arc::from(DEFAULT_FONT_BYTES)).unwrap();
        let before = realm.with_session(|session| {
            render_with_font(session.display_list(32, 32, &font).unwrap(), 32, 32, 1.0, false, &font).unwrap()
        });
        engine.eval_value("document.querySelector('head').innerHTML = '<style>div{width:20px;height:20px;background:blue}</style>'").unwrap().ok().unwrap();
        assert!(realm.session.borrow().document().mutations().iter().any(|m| matches!(m.kind, lumen_html::MutationKind::Tree { styles_changed: true, .. })));
        realm.with_session(|session| {
            let head = selector::query_selector(session.document(), session.document().root(), "head").unwrap().unwrap();
            assert_eq!(html::inner_html(session.document(), head).unwrap(), "<style>div{width:20px;height:20px;background:blue}</style>");
        });
        let after = realm.with_session(|session| {
            render_with_font(session.display_list(32, 32, &font).unwrap(), 32, 32, 1.0, false, &font).unwrap()
        });
        assert_ne!(&before.pixels[..4], &after.pixels[..4]);
    }

    #[test]
    fn document_lookup_and_character_data_use_shared_nodes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div id=app>x</div>", 64).unwrap();
        let result = engine.eval_value("const app = document.getElementById('app'); const text = app.firstChild; const comment = document.createComment('note'); app.appendChild(comment); text.nodeValue = 'y'; app.className = 'active'; app === document.querySelector('#app') && app.ownerDocument === document && window.document === document && app.className === 'active' && text.nodeValue === 'y' && app.textContent === 'y' && comment.nodeType === 8 && comment.nodeValue === 'note'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn repeated_inner_html_reuses_arena_slots_and_keeps_live_detached_nodes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app><span>old</span></div>", 32).unwrap();
        let result = engine.eval_value("const app = document.getElementById('app'); const old = app.firstChild; for (let i = 0; i < 100; i++) app.innerHTML = '<b>new</b>'; old.textContent === 'old' && old.parentNode === null && app.firstChild.nodeName === 'B'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
        assert!(realm.with_session(|session| session.document().node_count()) <= 12);
    }

    #[test]
    fn namespace_creation_keeps_svg_casing() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 64).unwrap();
        let result = engine.eval_value("const svg = document.createElementNS('http://www.w3.org/2000/svg', 'linearGradient'); document.querySelector('main').appendChild(svg); svg.nodeName === 'linearGradient' && document.createElementNS('http://www.w3.org/1999/xhtml', 'DIV').nodeName === 'DIV'").unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn frame_reaps_detached_nodes_after_wrapper_collection() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=app><span>old</span></div>", 32).unwrap();
        engine.eval_value("const app = document.getElementById('app'); (function () { const old = app.firstChild; app.innerHTML = '<b>new</b>'; })()").unwrap().ok().unwrap();
        engine.collect_garbage();
        let live = realm.with_session(|session| session.document().node_count());
        assert!(live <= 8, "{live} live nodes");
    }
}
