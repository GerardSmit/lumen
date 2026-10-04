use super::editing_history::{Composition, EditSnapshot, MAX_CONTROL_BYTES};
use super::*;
use lumen_common::ucd::{next_grapheme_boundary, previous_grapheme_boundary};

fn disabled_form_control(document: &lumen_html::Document, node: NodeId) -> OpResult<bool> {
    let NodeKind::Element {
        namespace: Namespace::Html,
        name,
        attributes,
    } = document.kind(node).map_err(dom_error)?
    else {
        return Ok(false);
    };
    let disableable = matches!(
        name.as_str(),
        "button" | "fieldset" | "input" | "optgroup" | "option" | "select" | "textarea"
    );
    if !disableable {
        return Ok(false);
    }
    if attributes.iter().any(|(name, _)| name == "disabled") {
        return Ok(true);
    }

    let mut ancestor = document.parent(node).map_err(dom_error)?;
    while let Some(fieldset) = ancestor {
        let disabled = matches!(
            document.kind(fieldset).map_err(dom_error)?,
            NodeKind::Element {
                namespace: Namespace::Html,
                name,
                attributes,
            } if name.as_str() == "fieldset" && attributes.iter().any(|(name, _)| name == "disabled")
        );
        if disabled {
            let mut first_legend = None;
            let mut child = document.first_child(fieldset).map_err(dom_error)?;
            while let Some(id) = child {
                if matches!(
                    document.kind(id).map_err(dom_error)?,
                    NodeKind::Element {
                        namespace: Namespace::Html,
                        name,
                        ..
                    } if name.as_str() == "legend"
                ) {
                    first_legend = Some(id);
                    break;
                }
                child = document.next_sibling(id).map_err(dom_error)?;
            }
            let mut current = Some(node);
            let mut exempt = false;
            while let Some(id) = current {
                if Some(id) == first_legend {
                    exempt = true;
                    break;
                }
                if id == fieldset {
                    break;
                }
                current = document.parent(id).map_err(dom_error)?;
            }
            if !exempt {
                return Ok(true);
            }
        }
        ancestor = document.parent(fieldset).map_err(dom_error)?;
    }
    Ok(false)
}

fn byte_offset(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units + ch.len_utf16() > offset {
            return byte;
        }
        units += ch.len_utf16();
    }
    text.len()
}

fn byte_offset_ceil(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        units += ch.len_utf16();
        if units >= offset {
            return byte + ch.len_utf8();
        }
    }
    text.len()
}

impl DomRealm {
    pub fn composition_range(&self, node: NodeId) -> Option<(usize, usize)> {
        self.editing
            .borrow()
            .composition(node)
            .map(|composition| (composition.mark_start, composition.mark_end))
    }

    fn edit_snapshot(&self, node: NodeId) -> OpResult<EditSnapshot> {
        let value = self.control_value(node)?;
        if value.len() > MAX_CONTROL_BYTES {
            return Err(OpError::new(
                "RangeError",
                "text control exceeds the editing limit",
            ));
        }
        let (start, end, direction) = self.selection(node)?;
        Ok(EditSnapshot {
            value,
            start,
            end,
            direction,
        })
    }

    /// Return bounded text state for a native InputHandler without first cloning
    /// an arbitrarily large control value into the host event bridge.
    pub fn host_text_input_state(
        &self,
        node: NodeId,
    ) -> Option<(String, usize, usize, bool, Option<(usize, usize)>)> {
        let value = {
            let session = self.session.borrow();
            let document = session.document();
            let NodeKind::Element {
                name, attributes, ..
            } = document.kind(node).ok()?
            else {
                return None;
            };
            if !matches!(name.as_str(), "input" | "textarea") {
                return None;
            }
            if let Some((_, value)) = attributes.iter().find(|(key, _)| key == "value") {
                if value.len() > MAX_CONTROL_BYTES {
                    return None;
                }
                value.clone()
            } else if name == "textarea" {
                let mut value = String::new();
                let mut pending = Vec::new();
                if let Some(child) = document.first_child(node).ok().flatten() {
                    pending.try_reserve(1).ok()?;
                    pending.push(child);
                }
                while let Some(current) = pending.pop() {
                    match document.kind(current).ok()? {
                        NodeKind::Text(text) => {
                            if value.len().saturating_add(text.len()) > MAX_CONTROL_BYTES {
                                return None;
                            }
                            value.try_reserve(text.len()).ok()?;
                            value.push_str(text);
                        }
                        NodeKind::Element { .. } | NodeKind::DocumentFragment => {
                            if let Some(sibling) = document.next_sibling(current).ok().flatten() {
                                pending.try_reserve(1).ok()?;
                                pending.push(sibling);
                            }
                            if let Some(child) = document.first_child(current).ok().flatten() {
                                pending.try_reserve(1).ok()?;
                                pending.push(child);
                            }
                            continue;
                        }
                        _ => {}
                    }
                    if let Some(sibling) = document.next_sibling(current).ok().flatten() {
                        pending.try_reserve(1).ok()?;
                        pending.push(sibling);
                    }
                }
                value
            } else {
                String::new()
            }
        };
        let length = value.encode_utf16().count();
        let (start, end, direction) =
            self.selections
                .borrow()
                .get(&node)
                .cloned()
                .unwrap_or((length, length, "none".into()));
        let start = start.min(length);
        let end = end.min(length).max(start);
        Some((
            value,
            start,
            end,
            direction == "backward",
            self.composition_range(node),
        ))
    }

    fn edit_control_info(&self, node: NodeId) -> OpResult<Option<(bool, Option<usize>, bool)>> {
        if disabled_form_control(&self.session.borrow().document(), node)? {
            return Ok(None);
        }
        let session = self.session.borrow();
        let NodeKind::Element {
            name, attributes, ..
        } = session.document().kind(node).map_err(dom_error)?
        else {
            return Ok(None);
        };
        let attr = |key: &str| {
            attributes
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.as_str())
        };
        if !matches!(name.as_str(), "input" | "textarea")
            || (name == "input"
                && !matches!(
                    attr("type").unwrap_or("text"),
                    "text" | "search" | "email" | "url" | "tel" | "password" | "number"
                ))
        {
            return Ok(None);
        }
        Ok(Some((
            name == "textarea",
            attr("maxlength").and_then(|value| value.parse::<usize>().ok()),
            attr("readonly").is_some(),
        )))
    }

    fn prepare_replacement(
        &self,
        before: &EditSnapshot,
        range: (usize, usize),
        inserted: &str,
        maximum: Option<usize>,
    ) -> Option<(String, usize, usize)> {
        if inserted.len() > MAX_CONTROL_BYTES {
            return None;
        }
        let length = before.value.encode_utf16().count();
        let start = range.0.min(length);
        let end = range.1.min(length).max(start);
        let left = byte_offset(&before.value, start);
        let right = byte_offset_ceil(&before.value, end);
        let normalized_start = before.value[..left].encode_utf16().count();
        let mut replacement = String::new();
        replacement
            .try_reserve(before.value.len().saturating_add(inserted.len()))
            .ok()?;
        replacement.push_str(&before.value[..left]);
        replacement.push_str(inserted);
        let caret = replacement.encode_utf16().count();
        replacement.push_str(&before.value[right..]);
        if replacement.len() > MAX_CONTROL_BYTES
            || maximum.is_some_and(|maximum| replacement.encode_utf16().count() > maximum)
        {
            return None;
        }
        Some((replacement, caret, normalized_start))
    }

    fn write_control_value(
        &self,
        node: NodeId,
        value: &str,
        start: usize,
        end: usize,
        direction: &str,
    ) -> OpResult<()> {
        forms::capture_defaults(
            &mut self.forms.borrow_mut(),
            self.session.borrow().document(),
            node,
        );
        self.session
            .borrow_mut()
            .document_mut()
            .set_attribute(node, "value", value)
            .map_err(dom_error)?;
        self.set_selection(node, start, end, direction)
    }

    fn apply_text_edit(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        before: EditSnapshot,
        range: (usize, usize),
        inserted: &str,
        input_type: &str,
        data: Option<&str>,
        cancelable: bool,
        composing: bool,
        record_history: bool,
    ) -> OpResult<bool> {
        let Some((_, maximum, readonly)) = self.edit_control_info(node)? else {
            return Ok(false);
        };
        if readonly {
            return Ok(false);
        }
        let Some((replacement, caret, mark_start)) =
            self.prepare_replacement(&before, range, inserted, maximum)
        else {
            return Ok(false);
        };
        let data = data.map_or(Value::Null, Value::str);
        let properties = [
            ("inputType", Value::str(input_type)),
            ("data", data),
            ("isComposing", Value::Bool(composing)),
        ];
        let epoch = self.value_epoch();
        if !self.dispatch(ctx, node, "beforeinput", true, cancelable, &properties)? {
            return Ok(false);
        }
        let after_beforeinput = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after_beforeinput.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        if after_beforeinput != before {
            return Ok(false);
        }
        if !matches!(self.edit_control_info(node)?, Some((_, _, false))) {
            return Ok(false);
        }
        self.write_control_value(node, &replacement, caret, caret, "none")?;
        let expected = EditSnapshot {
            value: replacement,
            start: caret,
            end: caret,
            direction: "none".into(),
        };
        self.dispatch(ctx, node, "input", true, false, &properties)?;
        let after = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after.value != expected.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(true);
        }
        if record_history {
            self.editing.borrow_mut().record(node, before, after);
        } else {
            let has_composition = self.editing.borrow().composition(node).is_some();
            if has_composition {
                let mark_end = mark_start + inserted.encode_utf16().count();
                self.editing.borrow_mut().update_composition(
                    node,
                    mark_start,
                    mark_end.max(mark_start),
                );
            }
        }
        Ok(true)
    }

    pub fn host_insert_text(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        text: &str,
    ) -> OpResult<bool> {
        self.host_insert_text_at(ctx, node, text, None)
    }

    pub fn host_insert_text_at(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        text: &str,
        replacement_range: Option<(usize, usize)>,
    ) -> OpResult<bool> {
        if text.is_empty() {
            return Ok(false);
        }
        let before = self.edit_snapshot(node)?;
        let range = replacement_range.unwrap_or((before.start, before.end));
        self.apply_text_edit(
            ctx,
            node,
            before.clone(),
            range,
            text,
            "insertText",
            Some(text),
            true,
            false,
            true,
        )
    }

    pub fn host_paste(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId, text: &str) -> OpResult<bool> {
        if text.len() > MAX_CONTROL_BYTES {
            return Ok(false);
        }
        let Some((_, _, readonly)) = self.edit_control_info(node)? else {
            return Ok(false);
        };
        if readonly {
            return Ok(false);
        }
        let before = self.edit_snapshot(node)?;
        let epoch = self.value_epoch();
        // The clipboard itself belongs to the host. This API accepts only the
        // plain-text payload already supplied by that host.
        if !self.dispatch(ctx, node, "paste", true, true, &[])? {
            return Ok(false);
        }
        let after_paste = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after_paste.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        if after_paste != before {
            return Ok(false);
        }
        self.apply_text_edit(
            ctx,
            node,
            before.clone(),
            (before.start, before.end),
            text,
            "insertFromPaste",
            Some(text),
            true,
            false,
            true,
        )
    }

    pub fn host_undo(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<bool> {
        self.apply_history(ctx, node, false)
    }

    pub fn host_redo(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<bool> {
        self.apply_history(ctx, node, true)
    }

    fn apply_history(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId, redo: bool) -> OpResult<bool> {
        if !matches!(self.edit_control_info(node)?, Some((_, _, false))) {
            return Ok(false);
        }
        let before = self.edit_snapshot(node)?;
        let Some(target) = self.editing.borrow_mut().target(node, &before, redo) else {
            return Ok(false);
        };
        let epoch = self.value_epoch();
        let input_type = if redo { "historyRedo" } else { "historyUndo" };
        let properties = [
            ("inputType", Value::str(input_type)),
            ("data", Value::Null),
            ("isComposing", Value::Bool(false)),
        ];
        if !self.dispatch(ctx, node, "beforeinput", true, true, &properties)? {
            return Ok(false);
        }
        let after_beforeinput = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after_beforeinput.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        if after_beforeinput != before {
            return Ok(false);
        }
        if !matches!(self.edit_control_info(node)?, Some((_, _, false))) {
            return Ok(false);
        }
        self.write_control_value(
            node,
            &target.value,
            target.start,
            target.end,
            &target.direction,
        )?;
        self.dispatch(ctx, node, "input", true, false, &properties)?;
        let after = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after.value != target.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(true);
        }
        self.editing.borrow_mut().commit_history(node, redo);
        Ok(true)
    }

    pub fn host_composition_start(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<bool> {
        if self.editing.borrow().composition(node).is_some() {
            return Ok(true);
        }
        if !matches!(self.edit_control_info(node)?, Some((_, _, false))) {
            return Ok(false);
        }
        let previous = self.editing.borrow().active_composition_node();
        if let Some(previous) = previous {
            self.host_composition_end(ctx, previous)?;
        }
        let before = self.edit_snapshot(node)?;
        let epoch = self.value_epoch();
        let properties = [("data", Value::str(""))];
        self.dispatch(ctx, node, "compositionstart", true, false, &properties)?;
        let after = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        let start = after.start;
        let end = after.end;
        self.editing.borrow_mut().begin_composition(Composition {
            node,
            before,
            mark_start: start,
            mark_end: end,
        });
        Ok(true)
    }

    pub fn host_composition_update(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        text: &str,
        replacement_range: Option<(usize, usize)>,
        selected_range: Option<(usize, usize)>,
    ) -> OpResult<bool> {
        if text.len() > MAX_CONTROL_BYTES {
            return Ok(false);
        }
        let has_composition = self.editing.borrow().composition(node).is_some();
        if !has_composition && !self.host_composition_start(ctx, node)? {
            return Ok(false);
        }
        let Some(composition) = self.editing.borrow().composition(node) else {
            return Ok(false);
        };
        let Some((_, maximum, readonly)) = self.edit_control_info(node)? else {
            self.editing.borrow_mut().invalidate(node);
            return Ok(false);
        };
        if readonly {
            self.editing.borrow_mut().invalidate(node);
            return Ok(false);
        }
        let before = self.edit_snapshot(node)?;
        let range = replacement_range.unwrap_or((composition.mark_start, composition.mark_end));
        let Some((replacement, _caret, mark_start)) =
            self.prepare_replacement(&before, range, text, maximum)
        else {
            return Ok(false);
        };
        let epoch = self.value_epoch();
        let data = Value::str(text);
        let event_properties = [("data", data.clone())];
        self.dispatch(
            ctx,
            node,
            "compositionupdate",
            true,
            false,
            &event_properties,
        )?;
        let after_composition_event = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after_composition_event.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        if after_composition_event != before {
            return Ok(false);
        }
        let properties = [
            ("inputType", Value::str("insertCompositionText")),
            ("data", data),
            ("isComposing", Value::Bool(true)),
        ];
        self.dispatch(ctx, node, "beforeinput", true, false, &properties)?;
        let after_beforeinput = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after_beforeinput.value != before.value {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        if after_beforeinput != before {
            return Ok(false);
        }
        let mark_end = mark_start + text.encode_utf16().count();
        let (selection_start, selection_end) =
            selected_range.map_or((mark_end, mark_end), |(start, end)| {
                let length = text.encode_utf16().count();
                (mark_start + start.min(length), mark_start + end.min(length))
            });
        self.write_control_value(node, &replacement, selection_start, selection_end, "none")?;
        self.dispatch(ctx, node, "input", true, false, &properties)?;
        let after = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) || after.value != replacement {
            self.invalidate_editing_for_value_change(node);
            return Ok(true);
        }
        self.editing
            .borrow_mut()
            .update_composition(node, mark_start, mark_end);
        Ok(true)
    }

    pub fn host_composition_end(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<bool> {
        let Some(composition) = self.editing.borrow().composition(node) else {
            return Ok(false);
        };
        let before_end = self.edit_snapshot(node)?;
        let start = byte_offset(&before_end.value, composition.mark_start);
        let end = byte_offset(&before_end.value, composition.mark_end);
        let data = before_end.value[start.min(end)..end.max(start)].to_owned();
        let epoch = self.value_epoch();
        let properties = [("data", Value::str(&data))];
        self.dispatch(ctx, node, "compositionend", true, false, &properties)?;
        let after = self.edit_snapshot(node)?;
        if self.value_changed_since(node, epoch) {
            self.invalidate_editing_for_value_change(node);
            return Ok(false);
        }
        self.editing.borrow_mut().finish_composition(node);
        let changed = composition.before.value != after.value;
        self.editing
            .borrow_mut()
            .record(node, composition.before, after);
        Ok(changed)
    }

    /// HTML focusability follows the rendered composed ancestry. Visibility
    /// is inherited but may be restored on a child; display and inertness
    /// exclude entire subtrees.
    pub(super) fn focus_rendered(&self, node: NodeId) -> OpResult<bool> {
        if disabled_form_control(&self.session.borrow().document(), node)? {
            return Ok(false);
        }
        let mut session = self.session.borrow_mut();
        let root = session.document().root();
        let mut current = node;
        loop {
            if current == root {
                return Ok(true);
            }
            let (element, inert, hidden_input) =
                match session.document().kind(current).map_err(dom_error)? {
                    NodeKind::Element {
                        name, attributes, ..
                    } => (
                        true,
                        attributes.iter().any(|(name, _)| name == "inert"),
                        current == node
                            && name == "input"
                            && attributes.iter().any(|(key, value)| {
                                key == "type" && value.eq_ignore_ascii_case("hidden")
                            }),
                    ),
                    _ => (false, false, false),
                };
            if inert || hidden_input {
                return Ok(false);
            }
            if element {
                let style = session.computed_style(current).map_err(|error| {
                    OpError::new(
                        "InvalidStateError",
                        format!("focus style failed: {error:?}"),
                    )
                })?;
                if style.display == lumen_html::css::Display::None
                    || (current == node && !style.visibility_visible)
                {
                    return Ok(false);
                }
            }
            let Some(parent) = session
                .document()
                .composed_parent(current)
                .map_err(dom_error)?
            else {
                return Ok(false);
            };
            let mut children = session
                .document()
                .composed_children_iter(parent)
                .map_err(dom_error)?;
            let mut included = false;
            while let Some(child) = children.next().map_err(dom_error)? {
                if child == current {
                    included = true;
                    break;
                }
            }
            if !included {
                return Ok(false);
            }
            current = parent;
        }
    }
    pub fn control_value(&self, node: NodeId) -> OpResult<String> {
        let session = self.session.borrow();
        let document = session.document();
        let NodeKind::Element {
            name, attributes, ..
        } = document.kind(node).map_err(dom_error)?
        else {
            return Err(OpError::new(
                "InvalidStateError",
                "selection requires a text control",
            ));
        };
        if !matches!(name.as_str(), "input" | "textarea") {
            return Err(OpError::new(
                "InvalidStateError",
                "selection requires a text control",
            ));
        }
        if let Some((_, value)) = attributes.iter().find(|(key, _)| key == "value") {
            return Ok(value.clone());
        }
        let mut value = String::new();
        if name == "textarea" {
            document.append_descendant_text(node, &mut value).map_err(dom_error)?;
        }
        Ok(value)
    }

    /// Selection offsets use UTF-16 code units, as in the DOM.
    pub fn selection(&self, node: NodeId) -> OpResult<(usize, usize, String)> {
        let length = self.control_value(node)?.encode_utf16().count();
        let (start, end, direction) =
            self.selections
                .borrow()
                .get(&node)
                .cloned()
                .unwrap_or((length, length, "none".into()));
        Ok((start.min(length), end.min(length), direction))
    }

    pub fn set_selection(
        &self,
        node: NodeId,
        start: usize,
        end: usize,
        direction: &str,
    ) -> OpResult<()> {
        let length = self.control_value(node)?.encode_utf16().count();
        let end = end.min(length);
        self.selections.borrow_mut().insert(
            node,
            (
                start.min(end),
                end,
                if matches!(direction, "forward" | "backward") {
                    direction
                } else {
                    "none"
                }
                .into(),
            ),
        );
        Ok(())
    }

    pub fn focus_next(self: &Rc<Self>, ctx: &mut Ctx, backwards: bool) -> OpResult<()> {
        let mut order = Vec::new();
        {
            let session = self.session.borrow();
            let document = session.document();
            let root = document.root();
            let mut pending = vec![root];
            while let Some(node) = pending.pop() {
                if let NodeKind::Element {
                    name, attributes, ..
                } = document.kind(node).map_err(dom_error)?
                {
                    let attr = |key: &str| {
                        attributes
                            .iter()
                            .find(|(name, _)| name == key)
                            .map(|(_, value)| value.as_str())
                    };
                    let natural = matches!(
                        name.as_str(),
                        "input" | "textarea" | "button" | "select" | "summary"
                    ) || (matches!(name.as_str(), "a" | "area")
                        && attr("href").is_some())
                        || attr("contenteditable").is_some_and(|value| value != "false");
                    let index = attr("tabindex")
                        .and_then(|value| value.parse::<i32>().ok())
                        .unwrap_or(if natural { 0 } else { -1 });
                    if index >= 0 && !disabled_form_control(document, node)? {
                        order.push((if index == 0 { i32::MAX } else { index }, node));
                    }
                }
                let children = document.composed_children(node).map_err(dom_error)?;
                pending
                    .try_reserve(children.len())
                    .map_err(|_| OpError::new("RangeError", "focus traversal allocation failed"))?;
                pending.extend(children.into_iter().rev());
            }
        }
        let mut rendered = Vec::new();
        for entry in order {
            if self.focus_rendered(entry.1)? {
                rendered.push(entry);
            }
        }
        let mut order = rendered;
        order.sort_by_key(|entry| entry.0);
        if order.is_empty() {
            return self.focus(ctx, None);
        }
        let current = order
            .iter()
            .position(|(_, node)| Some(*node) == self.focused_node());
        let index = if backwards {
            current.map_or(order.len() - 1, |index| {
                (index + order.len() - 1) % order.len()
            })
        } else {
            current.map_or(0, |index| (index + 1) % order.len())
        };
        self.focus(ctx, Some(order[index].1))
    }

    pub(super) fn edit_control_key(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        properties: &[(&str, Value)],
    ) -> OpResult<()> {
        let get = |name: &str| {
            properties
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value)
        };
        let Some(Value::Str(key)) = get("key") else {
            return Ok(());
        };
        if key.as_str() == "Tab" {
            return self.focus_next(ctx, matches!(get("shiftKey"), Some(Value::Bool(true))));
        }
        if disabled_form_control(&self.session.borrow().document(), node)? {
            return Ok(());
        }
        let control = matches!(get("ctrlKey"), Some(Value::Bool(true)));
        let meta = matches!(get("metaKey"), Some(Value::Bool(true)));
        let alt = matches!(get("altKey"), Some(Value::Bool(true)));
        let shift = matches!(get("shiftKey"), Some(Value::Bool(true)));
        if (control || meta) && !alt && matches!(key.as_str(), "z" | "Z" | "y" | "Y") {
            if matches!(key.as_str(), "y" | "Y") || shift {
                self.host_redo(ctx, node)?;
            } else {
                self.host_undo(ctx, node)?;
            }
            return Ok(());
        }
        if ["ctrlKey", "altKey", "metaKey"]
            .iter()
            .any(|name| matches!(get(name), Some(Value::Bool(true))))
        {
            return Ok(());
        }
        let (multiline, maximum, readonly) = {
            let session = self.session.borrow();
            let NodeKind::Element {
                name, attributes, ..
            } = session.document().kind(node).map_err(dom_error)?
            else {
                return Ok(());
            };
            let attr = |key: &str| {
                attributes
                    .iter()
                    .find(|(name, _)| name == key)
                    .map(|(_, value)| value.as_str())
            };
            if !matches!(name.as_str(), "input" | "textarea") || attr("disabled").is_some() {
                return Ok(());
            }
            if name == "input"
                && !matches!(
                    attr("type").unwrap_or("text"),
                    "text" | "search" | "email" | "url" | "tel" | "password" | "number"
                )
            {
                return Ok(());
            }
            (
                name == "textarea",
                attr("maxlength").and_then(|value| value.parse::<usize>().ok()),
                attr("readonly").is_some(),
            )
        };
        let value = self.control_value(node)?;
        if value.len() > 64 * 1024 {
            return Ok(());
        }
        let (start, end, direction) = self.selection(node)?;
        let mut left = byte_offset(&value, start);
        let mut right = byte_offset(&value, end);
        if matches!(key.as_str(), "ArrowLeft" | "ArrowRight" | "Home" | "End") {
            let caret = if direction == "backward" { left } else { right };
            let next = match key.as_str() {
                "Home" => 0,
                "End" => value.len(),
                "ArrowLeft" if !shift && left != right => left,
                "ArrowRight" if !shift && left != right => right,
                "ArrowLeft" => previous_grapheme_boundary(&value, caret),
                _ => next_grapheme_boundary(&value, caret),
            };
            let offset = value[..next].encode_utf16().count();
            let anchor = if direction == "backward" { end } else { start };
            return if shift {
                self.set_selection(
                    node,
                    anchor.min(offset),
                    anchor.max(offset),
                    if offset < anchor {
                        "backward"
                    } else {
                        "forward"
                    },
                )
            } else {
                self.set_selection(node, offset, offset, "none")
            };
        }
        if readonly {
            return Ok(());
        }
        let (kind, inserted) = match key.as_str() {
            "Backspace" => {
                if left == right {
                    left = previous_grapheme_boundary(&value, left);
                }
                ("deleteContentBackward", "")
            }
            "Delete" => {
                if left == right {
                    right = next_grapheme_boundary(&value, right);
                }
                ("deleteContentForward", "")
            }
            "Enter" if multiline => ("insertLineBreak", "\n"),
            key if key.chars().count() == 1 && !key.chars().any(char::is_control) => {
                ("insertText", key)
            }
            _ => return Ok(()),
        };
        if left == right && inserted.is_empty() {
            return Ok(());
        }
        let left_utf16 = value[..left].encode_utf16().count();
        let right_utf16 = value[..right].encode_utf16().count();
        let data = (!inserted.is_empty()).then_some(inserted);
        let before = EditSnapshot {
            value,
            start,
            end,
            direction,
        };
        let _ = maximum;
        self.apply_text_edit(
            ctx,
            node,
            before,
            (left_utf16, right_utf16),
            inserted,
            kind,
            data,
            true,
            false,
            true,
        )?;
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
        realm
            .dispatch(
                engine.ctx(),
                node,
                "keydown",
                true,
                true,
                &[("key", Value::str("x"))],
            )
            .unwrap();
        assert_eq!(realm.control_value(node).unwrap(), "axéz");
        assert_eq!(realm.selection(node).unwrap().0, 2);
        realm.set_selection(node, 4, 4, "none").unwrap();
        realm
            .dispatch(
                engine.ctx(),
                node,
                "keydown",
                true,
                true,
                &[("key", Value::str("Backspace"))],
            )
            .unwrap();
        assert_eq!(realm.control_value(node).unwrap(), "axz");
        assert_eq!(realm.selection(node).unwrap().0, 2);
    }

    #[test]
    fn paste_beforeinput_and_history_events_follow_host_order_and_cancel_rules() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field value=x>", 64).unwrap();
        engine.eval_value("const field=document.getElementById('field');field.focus();const events=[];for(const type of ['paste','beforeinput','input'])field.addEventListener(type,e=>events.push(type+':'+(e.inputType||'')+':'+e.cancelable));").unwrap().ok().unwrap();
        let node = realm.focused_node().unwrap();
        assert!(realm.host_paste(engine.ctx(), node, "y").unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "xy");
        assert!(
            matches!(engine.eval_value("events.join('|')").unwrap().ok().unwrap(), Value::Str(value) if value.as_str() == "paste::true|beforeinput:insertFromPaste:true|input:insertFromPaste:false")
        );
        engine.eval_value("field.addEventListener('beforeinput',e=>{if(e.inputType==='historyUndo')e.preventDefault()})").unwrap().ok().unwrap();
        assert!(!realm.host_undo(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "xy");
        assert!(realm.host_redo(engine.ctx(), node).unwrap() == false);
        assert_eq!(realm.control_value(node).unwrap(), "xy");
    }

    #[test]
    fn composition_updates_are_non_cancelable_and_commit_as_one_undo_step() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field>", 64).unwrap();
        engine.eval_value("const field=document.getElementById('field');field.value='a😀z';field.setSelectionRange(1,3);field.focus();const events=[];for(const type of ['compositionstart','compositionupdate','beforeinput','input','compositionend'])field.addEventListener(type,e=>{if(e.inputType==='insertCompositionText')e.preventDefault();events.push(type+':'+(e.inputType||'')+':'+e.cancelable+':'+e.defaultPrevented)});").unwrap().ok().unwrap();
        let node = realm.focused_node().unwrap();
        assert!(realm.host_composition_start(engine.ctx(), node).unwrap());
        assert!(realm
            .host_composition_update(engine.ctx(), node, "e", None, None)
            .unwrap());
        assert!(realm
            .host_composition_update(engine.ctx(), node, "é", None, Some((0, 1)))
            .unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "aéz");
        assert!(realm.host_composition_end(engine.ctx(), node).unwrap());
        assert!(
            matches!(engine.eval_value("events.join('|')").unwrap().ok().unwrap(), Value::Str(value) if value.as_str() == "compositionstart::false:false|compositionupdate::false:false|beforeinput:insertCompositionText:false:false|input:insertCompositionText:false:false|compositionupdate::false:false|beforeinput:insertCompositionText:false:false|input:insertCompositionText:false:false|compositionend::false:false")
        );
        assert!(realm.host_undo(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "a😀z");
        assert_eq!(realm.selection(node).unwrap(), (1, 3, "none".into()));
        assert!(realm.host_redo(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "aéz");
    }

    #[test]
    fn programmatic_value_mutation_invalidates_history_and_reentrant_writes_abort_default() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field>", 64).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');field.value='a';field.focus();",
            )
            .unwrap()
            .ok()
            .unwrap();
        let node = realm.focused_node().unwrap();
        assert!(realm.host_insert_text(engine.ctx(), node, "b").unwrap());
        engine
            .eval_value("field.value='programmatic'")
            .unwrap()
            .ok()
            .unwrap();
        assert!(!realm.host_undo(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "programmatic");

        engine.eval_value("field.value='stable';globalThis.__reentrantBeforeInputCalls=0;field.addEventListener('beforeinput',e=>{globalThis.__reentrantBeforeInputCalls++;if(e.inputType==='insertText')field.value=field.value},{once:true})").unwrap().ok().unwrap();
        let epoch_before_reentrant_edit = realm.value_epoch();
        let accepted = realm.host_insert_text(engine.ctx(), node, "x").unwrap();
        assert!(
            matches!(engine.eval_value("__reentrantBeforeInputCalls").unwrap().ok().unwrap(), Value::Num(value) if value == 1.0),
            "the once beforeinput listener did not run exactly once"
        );
        assert!(
            realm.value_epoch() > epoch_before_reentrant_edit,
            "a same-value IDL assignment from beforeinput did not advance the programmatic-write epoch"
        );
        assert!(
            !accepted,
            "a reentrant value write did not abort the pending edit"
        );
        assert_eq!(realm.control_value(node).unwrap(), "stable");
    }

    #[test]
    fn unrelated_control_writes_do_not_abort_an_edit() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field><input id=other>", 64).unwrap();
        engine.eval_value("const field=document.getElementById('field');const other=document.getElementById('other');field.focus();field.addEventListener('beforeinput',()=>{other.value='changed'})").unwrap().ok().unwrap();
        let node = realm.focused_node().unwrap();
        assert!(realm.host_insert_text(engine.ctx(), node, "a").unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "a");
        assert!(
            matches!(engine.eval_value("other.value").unwrap().ok().unwrap(), Value::Str(value) if value.as_str() == "changed")
        );
        realm.set_selection(node, 0, 0, "none").unwrap();
        assert!(realm.host_undo(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "");
    }

    #[test]
    fn native_text_state_uses_utf16_selection_and_refuses_oversized_clone() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field value='a😀z'>", 64).unwrap();
        let node = realm.with_session(|session| {
            selector::query_selector(session.document(), session.document().root(), "#field")
                .unwrap()
                .unwrap()
        });
        realm.set_selection(node, 1, 3, "backward").unwrap();
        assert_eq!(
            realm.host_text_input_state(node),
            Some(("a😀z".into(), 1, 3, true, None))
        );
        let oversized = "x".repeat(MAX_CONTROL_BYTES + 1);
        realm.with_session(|session| {
            session
                .document_mut()
                .set_attribute(node, "value", &oversized)
                .unwrap();
        });
        assert!(realm.host_text_input_state(node).is_none());
    }

    #[test]
    fn native_replacement_normalizes_a_split_surrogate_to_the_codepoint() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field value='a😀z'>", 64).unwrap();
        engine.eval_value("const field=document.getElementById('field');field.focus();field.setSelectionRange(2,2)").unwrap().ok().unwrap();
        let node = realm.focused_node().unwrap();
        assert!(realm
            .host_insert_text_at(engine.ctx(), node, "X", Some((2, 2)))
            .unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "aXz");
        assert_eq!(realm.selection(node).unwrap().0, 2);
    }

    #[test]
    fn host_edits_obey_maxlength_and_readonly_without_claiming_clipboard_access() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=field maxlength=2 value=ab>", 64).unwrap();
        engine
            .eval_value("const field=document.getElementById('field');field.focus()")
            .unwrap()
            .ok()
            .unwrap();
        let node = realm.focused_node().unwrap();
        assert!(!realm.host_insert_text(engine.ctx(), node, "c").unwrap());
        assert!(!realm.host_paste(engine.ctx(), node, "c").unwrap());
        engine
            .eval_value("field.readOnly=true")
            .unwrap()
            .ok()
            .unwrap();
        assert!(!realm.host_insert_text(engine.ctx(), node, "a").unwrap());
        assert!(!realm.host_composition_start(engine.ctx(), node).unwrap());
        assert_eq!(realm.control_value(node).unwrap(), "ab");
    }

    #[test]
    fn focus_excludes_hidden_and_inert_composed_subtrees_and_allows_visible_overrides() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<style>.gone{display:none}.invisible{visibility:hidden}.visible{visibility:visible}[hidden].shown{display:block}</style><div class=gone><input id=gone></div><div inert><input id=inert></div><div hidden><input id=hidden></div><input id=typehidden type=HIDDEN style='display:block!important'><div class=invisible><input id=invisible><input id=restored class=visible></div><input id=shown hidden class=shown><input id=last>", 128).unwrap();
        engine.eval_value("for(const id of ['gone','inert','hidden','typehidden','invisible']){document.getElementById(id).focus();if(document.activeElement.id===id)throw Error(id);}").unwrap().ok().unwrap();
        for id in ["restored", "shown", "last"] {
            realm.focus_next(engine.ctx(), false).unwrap();
            let check = format!("document.activeElement.id === '{id}'");
            assert!(
                matches!(
                    engine.eval_value(&check).unwrap().ok().unwrap(),
                    Value::Bool(true)
                ),
                "{id}"
            );
        }
        engine.eval_value("const host=document.createElement('div');host.id='host';document.body.appendChild(host);host.appendChild(document.createElement('input')).id='unslotted';const shadow=host.attachShadow({mode:'closed'});shadow.innerHTML='<input id=shadow>';globalThis.shadowInput=shadow.firstChild;document.getElementById('unslotted').focus();if(document.activeElement.id==='unslotted')throw Error('unslotted focus');").unwrap().ok().unwrap();
        realm.focus_next(engine.ctx(), false).unwrap();
        assert!(matches!(engine.eval_value("document.activeElement.id==='host' && shadowInput.getRootNode().activeElement===shadowInput").unwrap().ok().unwrap(), Value::Bool(true)));
    }

    #[test]
    fn tab_order_and_cancelable_defaults() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<input id=zero><button id=two tabindex=2></button><button id=one tabindex=1></button><input id=skip tabindex=-1><input disabled><input type=hidden>", 128).unwrap();
        realm.focus_next(engine.ctx(), false).unwrap();
        assert!(matches!(
            engine
                .eval_value("document.activeElement.id === 'one'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        realm.focus_next(engine.ctx(), false).unwrap();
        assert!(matches!(
            engine
                .eval_value("document.activeElement.id === 'two'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        realm.focus_next(engine.ctx(), false).unwrap();
        let node = realm.focused_node().unwrap();
        engine
            .eval_value(
                "document.activeElement.addEventListener('keydown', e => e.preventDefault());",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(!realm
            .dispatch(
                engine.ctx(),
                node,
                "keydown",
                true,
                true,
                &[("key", Value::str("Tab"))]
            )
            .unwrap());
        assert_eq!(realm.focused_node(), Some(node));
        realm.focus_next(engine.ctx(), true).unwrap();
        assert!(matches!(
            engine
                .eval_value("document.activeElement.id === 'two'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
    }
}
