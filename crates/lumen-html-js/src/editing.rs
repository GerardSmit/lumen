use super::*;
use lumen_common::ucd::{next_grapheme_boundary, previous_grapheme_boundary};

fn byte_offset(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units + ch.len_utf16() > offset { return byte; }
        units += ch.len_utf16();
    }
    text.len()
}

impl DomRealm {
    pub fn control_value(&self, node: NodeId) -> OpResult<String> {
        let session = self.session.borrow();
        let document = session.document();
        let NodeKind::Element { name, attributes, .. } = document.kind(node).map_err(dom_error)? else { return Err(OpError::new("InvalidStateError", "selection requires a text control")); };
        if !matches!(name.as_str(), "input" | "textarea") { return Err(OpError::new("InvalidStateError", "selection requires a text control")); }
        if let Some((_, value)) = attributes.iter().find(|(key, _)| key == "value") { return Ok(value.clone()); }
        let mut value = String::new();
        if name == "textarea" { text_content(document, node, &mut value).map_err(dom_error)?; }
        Ok(value)
    }

    /// Selection offsets use UTF-16 code units, as in the DOM.
    pub fn selection(&self, node: NodeId) -> OpResult<(usize, usize, String)> {
        let length = self.control_value(node)?.encode_utf16().count();
        let (start, end, direction) = self.selections.borrow().get(&node).cloned().unwrap_or((length, length, "none".into()));
        Ok((start.min(length), end.min(length), direction))
    }

    pub fn set_selection(&self, node: NodeId, start: usize, end: usize, direction: &str) -> OpResult<()> {
        let length = self.control_value(node)?.encode_utf16().count();
        let end = end.min(length);
        self.selections.borrow_mut().insert(node, (start.min(end), end, if matches!(direction, "forward" | "backward") { direction } else { "none" }.into()));
        Ok(())
    }

    pub fn focus_next(self: &Rc<Self>, ctx: &mut Ctx, backwards: bool) -> OpResult<()> {
        let mut order = Vec::new();
        {
            let session = self.session.borrow();
            let document = session.document();
            let root = document.root();
            let mut cursor = next_descendant(document, root, root).map_err(dom_error)?;
            while let Some(node) = cursor {
                if let NodeKind::Element { name, attributes, .. } = document.kind(node).map_err(dom_error)? {
                    let attr = |key: &str| attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
                    let natural = matches!(name.as_str(), "input" | "textarea" | "button" | "select" | "summary") || (matches!(name.as_str(), "a" | "area") && attr("href").is_some()) || attr("contenteditable").is_some_and(|value| value != "false");
                    let index = attr("tabindex").and_then(|value| value.parse::<i32>().ok()).unwrap_or(if natural { 0 } else { -1 });
                    if index >= 0 && attr("disabled").is_none() && attr("hidden").is_none() && !(name == "input" && attr("type") == Some("hidden")) { order.push((if index == 0 { i32::MAX } else { index }, node)); }
                }
                cursor = next_descendant(document, root, node).map_err(dom_error)?;
            }
        }
        order.sort_by_key(|entry| entry.0);
        if order.is_empty() { return self.focus(ctx, None); }
        let current = order.iter().position(|(_, node)| Some(*node) == self.focused_node());
        let index = if backwards { current.map_or(order.len() - 1, |index| (index + order.len() - 1) % order.len()) } else { current.map_or(0, |index| (index + 1) % order.len()) };
        self.focus(ctx, Some(order[index].1))
    }

    pub(super) fn edit_control_key(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId, properties: &[(&str, Value)]) -> OpResult<()> {
        let get = |name: &str| properties.iter().find(|(key, _)| *key == name).map(|(_, value)| value);
        let Some(Value::Str(key)) = get("key") else { return Ok(()); };
        if key.as_str() == "Tab" { return self.focus_next(ctx, matches!(get("shiftKey"), Some(Value::Bool(true)))); }
        if ["ctrlKey", "altKey", "metaKey"].iter().any(|name| matches!(get(name), Some(Value::Bool(true)))) { return Ok(()); }
        let (multiline, maximum, readonly) = {
            let session = self.session.borrow();
            let NodeKind::Element { name, attributes, .. } = session.document().kind(node).map_err(dom_error)? else { return Ok(()); };
            let attr = |key: &str| attributes.iter().find(|(name, _)| name == key).map(|(_, value)| value.as_str());
            if !matches!(name.as_str(), "input" | "textarea") || attr("disabled").is_some() { return Ok(()); }
            if name == "input" && !matches!(attr("type").unwrap_or("text"), "text" | "search" | "email" | "url" | "tel" | "password" | "number") { return Ok(()); }
            (name == "textarea", attr("maxlength").and_then(|value| value.parse::<usize>().ok()), attr("readonly").is_some())
        };
        let value = self.control_value(node)?;
        if value.len() > 64 * 1024 { return Ok(()); }
        let (start, end, direction) = self.selection(node)?;
        let mut left = byte_offset(&value, start);
        let mut right = byte_offset(&value, end);
        if matches!(key.as_str(), "ArrowLeft" | "ArrowRight" | "Home" | "End") {
            let shift = matches!(get("shiftKey"), Some(Value::Bool(true)));
            let caret = if direction == "backward" { left } else { right };
            let next = match key.as_str() { "Home" => 0, "End" => value.len(), "ArrowLeft" if !shift && left != right => left, "ArrowRight" if !shift && left != right => right, "ArrowLeft" => previous_grapheme_boundary(&value, caret), _ => next_grapheme_boundary(&value, caret) };
            let offset = value[..next].encode_utf16().count();
            let anchor = if direction == "backward" { end } else { start };
            return if shift { self.set_selection(node, anchor.min(offset), anchor.max(offset), if offset < anchor { "backward" } else { "forward" }) } else { self.set_selection(node, offset, offset, "none") };
        }
        if readonly { return Ok(()); }
        let (kind, inserted) = match key.as_str() {
            "Backspace" => { if left == right { left = previous_grapheme_boundary(&value, left); } ("deleteContentBackward", "") },
            "Delete" => { if left == right { right = next_grapheme_boundary(&value, right); } ("deleteContentForward", "") },
            "Enter" if multiline => ("insertLineBreak", "\n"),
            key if key.chars().count() == 1 && !key.chars().any(char::is_control) => ("insertText", key),
            _ => return Ok(()),
        };
        if left == right && inserted.is_empty() { return Ok(()); }
        let mut replacement = value[..left].to_owned();
        replacement.push_str(inserted);
        let caret = replacement.encode_utf16().count();
        replacement.push_str(&value[right..]);
        if replacement.len() > 64 * 1024 || (!inserted.is_empty() && maximum.is_some_and(|maximum| replacement.encode_utf16().count() > maximum)) { return Ok(()); }
        let properties = [("inputType", Value::str(kind)), ("data", if inserted.is_empty() { Value::Null } else { Value::str(inserted) })];
        if !self.dispatch(ctx, node, "beforeinput", true, true, &properties)? { return Ok(()); }
        if self.control_value(node)? != value || self.selection(node)? != (start, end, direction) { return Ok(()); }
        self.session.borrow_mut().document_mut().set_attribute(node, "value", &replacement).map_err(dom_error)?;
        self.set_selection(node, caret, caret, "none")?;
        self.dispatch(ctx, node, "input", true, false, &properties)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    #[test]
    fn selection_replaces_utf16_ranges_and_deletes_graphemes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input>", 64).unwrap();
        engine.eval_value("var field = document.querySelector('input'); field.value = 'a😀éz'; field.focus(); field.setSelectionRange(1,3);").unwrap().ok().unwrap();
        let node = realm.focused_node().unwrap();
        realm.dispatch(engine.ctx(), node, "keydown", true, true, &[("key", Value::str("x"))]).unwrap();
        assert_eq!(realm.control_value(node).unwrap(), "axéz");
        assert_eq!(realm.selection(node).unwrap().0, 2);
        realm.set_selection(node, 4, 4, "none").unwrap();
        realm.dispatch(engine.ctx(), node, "keydown", true, true, &[("key", Value::str("Backspace"))]).unwrap();
        assert_eq!(realm.control_value(node).unwrap(), "axz");
        assert_eq!(realm.selection(node).unwrap().0, 2);
    }

    #[test]
    fn tab_order_and_cancelable_defaults() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=zero><button id=two tabindex=2></button><button id=one tabindex=1></button><input id=skip tabindex=-1><input disabled><input type=hidden>", 128).unwrap();
        realm.focus_next(engine.ctx(), false).unwrap();
        assert!(matches!(engine.eval_value("document.activeElement.id === 'one'").unwrap().ok().unwrap(), Value::Bool(true)));
        realm.focus_next(engine.ctx(), false).unwrap();
        assert!(matches!(engine.eval_value("document.activeElement.id === 'two'").unwrap().ok().unwrap(), Value::Bool(true)));
        realm.focus_next(engine.ctx(), false).unwrap();
        let node = realm.focused_node().unwrap();
        engine.eval_value("document.activeElement.addEventListener('keydown', e => e.preventDefault());").unwrap().ok().unwrap();
        assert!(!realm.dispatch(engine.ctx(), node, "keydown", true, true, &[("key", Value::str("Tab"))]).unwrap());
        assert_eq!(realm.focused_node(), Some(node));
        realm.focus_next(engine.ctx(), true).unwrap();
        assert!(matches!(engine.eval_value("document.activeElement.id === 'two'").unwrap().ok().unwrap(), Value::Bool(true)));
    }
}
