use super::editing_history::{Composition, EditSnapshot, MAX_CONTROL_BYTES};
use super::*;
use lumen_common::ucd::{next_grapheme_boundary, previous_grapheme_boundary};

fn null_attribute<'a>(
    document: &'a lumen_html::Document,
    node: NodeId,
    name: &str,
) -> Option<&'a str> {
    document
        .get_attribute_ns_ref(node, None, name)
        .ok()
        .flatten()
}

fn disabled_form_control(document: &lumen_html::Document, node: NodeId) -> OpResult<bool> {
    let Some(name) = lumen_html::forms::html_element_local_name(document, node) else {
        return Ok(false);
    };
    let disableable = matches!(
        name,
        "button" | "fieldset" | "input" | "optgroup" | "option" | "select" | "textarea"
    );
    if !disableable {
        return Ok(false);
    }
    if null_attribute(document, node, "disabled").is_some() {
        return Ok(true);
    }

    let mut ancestor = document.parent(node).map_err(dom_error)?;
    while let Some(fieldset) = ancestor {
        let disabled = lumen_html::forms::html_element_local_name(document, fieldset)
            == Some("fieldset")
            && null_attribute(document, fieldset, "disabled").is_some();
        if disabled {
            let mut first_legend = None;
            let mut child = document.first_child(fieldset).map_err(dom_error)?;
            while let Some(id) = child {
                if lumen_html::forms::html_element_local_name(document, id) == Some("legend") {
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
        let width = if lumen_common::smuggle::smuggled(ch).is_some() {
            1
        } else {
            ch.len_utf16()
        };
        if units + width > offset {
            return byte;
        }
        units += width;
    }
    text.len()
}

fn byte_offset_ceil(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units >= offset {
            return byte;
        }
        units += if lumen_common::smuggle::smuggled(ch).is_some() {
            1
        } else {
            ch.len_utf16()
        };
        if units >= offset {
            return byte + ch.len_utf8();
        }
    }
    text.len()
}

fn normalize_textarea_value(value: &str) -> OpResult<String> {
    if value.len() > MAX_CONTROL_BYTES {
        return Err(OpError::new(
            "RangeError",
            "text control exceeds the editing limit",
        ));
    }
    let mut normalized = String::new();
    normalized
        .try_reserve_exact(value.len())
        .map_err(|_| OpError::new("RangeError", "text control allocation failed"))?;
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\r' {
            if characters.peek() == Some(&'\n') {
                characters.next();
            }
            normalized.push('\n');
        } else {
            normalized.push(character);
        }
    }
    Ok(normalized)
}

impl DomRealm {
    pub fn supports_text_selection(&self, node: NodeId) -> bool {
        lumen_html::forms::supports_text_selection(&self.session.borrow().document(), node)
    }

    /// Apply the IDL value setter and update the text-control caret only when
    /// the resulting API value actually changes. Textarea values are exposed
    /// with normalized line endings, so compare and store that form.
    pub fn set_control_value_and_selection(
        self: &Rc<Self>,
        node: NodeId,
        value: &str,
        is_textarea: bool,
    ) -> OpResult<()> {
        let normalized;
        let value = if is_textarea {
            normalized = normalize_textarea_value(value)?;
            normalized.as_str()
        } else {
            value
        };
        let previous = self.control_value(node)?;
        forms::set_control_value(self, &mut self.forms.borrow_mut(), node, value)?;
        if self.supports_text_selection(node) {
            let current = self.control_value(node)?;
            if current != previous {
                let end = lumen_common::smuggle::utf16_unit_len(&current);
                self.set_selection(node, end, end, "none")?;
            }
        }
        Ok(())
    }

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
        let live = {
            let state = self.forms.borrow();
            match super::forms::live_value(&state, node) {
                Some(value) if value.len() > MAX_CONTROL_BYTES => return None,
                Some(value) => Some(value.to_owned()),
                None => None,
            }
        };
        let value = if let Some(value) = live {
            value
        } else {
            let session = self.session.borrow();
            let document = session.document();
            let tag = lumen_html::forms::html_element_local_name(document, node)?;
            let NodeKind::Element { .. } = document.kind(node).ok()? else {
                return None;
            };
            if !matches!(tag, "input" | "textarea") {
                return None;
            }
            if tag == "input" {
                let raw = null_attribute(document, node, "value").unwrap_or("");
                if raw.len() > MAX_CONTROL_BYTES {
                    return None;
                }
                let value = lumen_html::forms::default_value(document, node)?;
                if value.len() > MAX_CONTROL_BYTES {
                    return None;
                }
                value
            } else {
                // Textarea's value is its live sidecar, or its current text
                // children. A `value` attribute is unrelated DOM content.
                let mut value = String::new();
                let mut pending = Vec::new();
                if let Some(child) = document.first_child(node).ok().flatten() {
                    pending.try_reserve(1).ok()?;
                    pending.push(child);
                }
                while let Some(current) = pending.pop() {
                    match document.kind(current).ok()? {
                        NodeKind::Text(text) | NodeKind::CData(text) => {
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
            }
        };
        let length = lumen_common::smuggle::utf16_unit_len(&value);
        let (start, end, direction) =
            self.selections
                .borrow()
                .get(&node)
                .cloned()
                .unwrap_or_else(|| {
                    let initial = self.initial_caret(node, length);
                    (initial, initial, "none".into())
                });
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
        let tag = lumen_html::forms::html_element_local_name(session.document(), node);
        let document = session.document();
        let NodeKind::Element { .. } = document.kind(node).map_err(dom_error)? else {
            return Ok(None);
        };
        let Some(tag) = tag else {
            return Ok(None);
        };
        let attr = |key: &str| null_attribute(document, node, key);
        if !matches!(tag, "input" | "textarea")
            || (tag == "input"
                && !matches!(
                    attr("type").unwrap_or("text").to_ascii_lowercase().as_str(),
                    "text" | "search" | "email" | "url" | "tel" | "password"
                ))
        {
            return Ok(None);
        }
        Ok(Some((
            tag == "textarea",
            attr("maxlength").and_then(lumen_html::forms::parse_nonnegative_integer),
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
        let length = lumen_common::smuggle::utf16_unit_len(&before.value);
        let start = range.0.min(length);
        let end = range.1.min(length).max(start);
        let left = byte_offset(&before.value, start);
        let right = byte_offset_ceil(&before.value, end);
        let normalized_start = lumen_common::smuggle::utf16_unit_len(&before.value[..left]);
        let mut replacement = String::new();
        replacement
            .try_reserve(before.value.len().saturating_add(inserted.len()))
            .ok()?;
        replacement.push_str(&before.value[..left]);
        replacement.push_str(inserted);
        let caret = lumen_common::smuggle::utf16_unit_len(&replacement);
        replacement.push_str(&before.value[right..]);
        if replacement.len() > MAX_CONTROL_BYTES
            || maximum.is_some_and(|maximum| {
                lumen_common::smuggle::utf16_unit_len(&replacement) > maximum
            })
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
        forms::set_control_value_from_user(self, &mut self.forms.borrow_mut(), node, value)?;
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
        self.apply_text_edit_with_keypress(
            ctx,
            node,
            before,
            range,
            inserted,
            input_type,
            data,
            cancelable,
            composing,
            record_history,
            false,
            None,
        )
    }

    fn apply_text_edit_with_keypress(
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
        trusted_input: bool,
        keypress: Option<&[(&str, Value)]>,
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
        let beforeinput_allowed = if trusted_input {
            self.dispatch_user_agent(ctx, node, "beforeinput", true, cancelable, &properties)?
        } else {
            self.dispatch(ctx, node, "beforeinput", true, cancelable, &properties)?
        };
        if !beforeinput_allowed {
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
        if let Some(properties) = keypress {
            if self.focused_node() != Some(node)
                || !self.dispatch_user_agent_keyboard_event(ctx, node, "keypress", properties)?
            {
                return Ok(false);
            }
            let after_keypress = self.edit_snapshot(node)?;
            if self.value_changed_since(node, epoch) || after_keypress.value != before.value {
                self.invalidate_editing_for_value_change(node);
                return Ok(false);
            }
            if after_keypress != after_beforeinput
                || !matches!(self.edit_control_info(node)?, Some((_, _, false)))
            {
                return Ok(false);
            }
        }
        self.write_control_value(node, &replacement, caret, caret, "none")?;
        // Record host provenance before dispatching `input`; a reentrant
        // script value setter clears it again through set_control_value.
        forms::mark_user_edited(&mut self.forms.borrow_mut(), node);
        let expected = EditSnapshot {
            value: replacement,
            start: caret,
            end: caret,
            direction: "none".into(),
        };
        if trusted_input {
            self.dispatch_user_agent(ctx, node, "input", true, false, &properties)?;
        } else {
            self.dispatch(ctx, node, "input", true, false, &properties)?;
        }
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
                let mark_end = mark_start + lumen_common::smuggle::utf16_unit_len(inserted);
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
        forms::mark_user_edited(&mut self.forms.borrow_mut(), node);
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
        let mark_end = mark_start + lumen_common::smuggle::utf16_unit_len(text);
        let (selection_start, selection_end) =
            selected_range.map_or((mark_end, mark_end), |(start, end)| {
                let length = lumen_common::smuggle::utf16_unit_len(text);
                (mark_start + start.min(length), mark_start + end.min(length))
            });
        self.write_control_value(node, &replacement, selection_start, selection_end, "none")?;
        forms::mark_user_edited(&mut self.forms.borrow_mut(), node);
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
                    NodeKind::Element { attributes, .. } => (
                        true,
                        attributes.iter().any(|(name, _)| name == "inert"),
                        current == node
                            && lumen_html::forms::html_element_local_name(
                                session.document(),
                                current,
                            ) == Some("input")
                            && null_attribute(session.document(), current, "type")
                                .is_some_and(|value| value.eq_ignore_ascii_case("hidden")),
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
    /// A textarea that was never written through the value setter or a selection API
    /// starts with its caret at the start; other text controls start at the end.
    fn initial_caret(&self, node: NodeId, length: usize) -> usize {
        let session = self.session.borrow();
        if lumen_html::forms::html_element_local_name(session.document(), node)
            == Some("textarea")
        {
            0
        } else {
            length
        }
    }

    pub fn control_value(&self, node: NodeId) -> OpResult<String> {
        let is_text_control = matches!(
            lumen_html::forms::html_element_local_name(&self.session.borrow().document(), node,),
            Some("input" | "textarea")
        );
        if !is_text_control {
            return Err(OpError::new(
                "InvalidStateError",
                "selection requires a text control",
            ));
        }
        super::forms::control_value(self, node)
    }

    /// Selection offsets use UTF-16 code units, as in the DOM.
    pub fn selection(&self, node: NodeId) -> OpResult<(usize, usize, String)> {
        let value = self.control_value(node)?;
        let length = lumen_common::smuggle::utf16_unit_len(&value);
        let (start, end, direction) =
            self.selections
                .borrow()
                .get(&node)
                .cloned()
                .unwrap_or_else(|| {
                    let initial = self.initial_caret(node, length);
                    (initial, initial, "none".into())
                });
        Ok((start.min(length), end.min(length), direction))
    }

    pub fn set_selection(
        &self,
        node: NodeId,
        start: usize,
        end: usize,
        direction: &str,
    ) -> OpResult<()> {
        let value = self.control_value(node)?;
        let length = lumen_common::smuggle::utf16_unit_len(&value);
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

    pub fn set_selection_and_queue_event(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        start: usize,
        end: usize,
        direction: &str,
    ) -> OpResult<()> {
        if !self.supports_text_selection(node) {
            return Err(error_reporting::dom_exception(
                ctx,
                "InvalidStateError",
                "text selection does not apply to this input type",
            ));
        }
        let before = self.selection(node)?;
        self.set_selection(node, start, end, direction)?;
        if self.selection(node)? != before {
            self.queue_select_event(ctx, node)?;
        }
        Ok(())
    }

    pub fn set_range_text(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        replacement: &str,
        start: Option<usize>,
        end: Option<usize>,
        use_current_selection: bool,
        selection_mode: &str,
    ) -> OpResult<()> {
        if !self.supports_text_selection(node) {
            return Err(error_reporting::dom_exception(
                ctx,
                "InvalidStateError",
                "setRangeText does not apply to this input type",
            ));
        }
        if !matches!(selection_mode, "select" | "start" | "end" | "preserve") {
            return Err(OpError::new(
                "TypeError",
                "invalid setRangeText selection mode",
            ));
        }

        let before = self.edit_snapshot(node)?;
        let old_units = lumen_common::smuggle::utf16_units(&before.value);
        let length = old_units.len();
        let current_selection = (before.start, before.end);
        let (requested_start, requested_end) = if use_current_selection {
            current_selection
        } else {
            (start.unwrap_or(0), end.unwrap_or(0))
        };
        forms::mark_value_dirty(self, &mut self.forms.borrow_mut(), node)?;
        if requested_start > requested_end {
            return Err(error_reporting::dom_exception(
                ctx,
                "IndexSizeError",
                "setRangeText start is greater than end",
            ));
        }
        let range = (requested_start.min(length), requested_end.min(length));
        let is_textarea = {
            let session = self.session.borrow();
            lumen_html::forms::html_element_local_name(session.document(), node) == Some("textarea")
        };
        if replacement.len() > MAX_CONTROL_BYTES {
            return Err(OpError::new(
                "RangeError",
                "setRangeText result exceeds the text-control editing limit",
            ));
        }
        let replacement_units = lumen_common::smuggle::utf16_units(replacement);
        let new_unit_len = old_units
            .len()
            .checked_sub(range.1 - range.0)
            .and_then(|remaining| remaining.checked_add(replacement_units.len()))
            .ok_or_else(|| {
                OpError::new(
                    "RangeError",
                    "setRangeText result exceeds the text-control editing limit",
                )
            })?;
        // The UTF-16 scratch list may be larger than its source strings, but
        // keep it bounded independently of the final byte-size check.
        if new_unit_len > MAX_CONTROL_BYTES.saturating_mul(2) {
            return Err(OpError::new(
                "RangeError",
                "setRangeText result exceeds the text-control editing limit",
            ));
        }
        let mut new_units = old_units;
        new_units
            .try_reserve(replacement_units.len())
            .map_err(|_| OpError::new("RangeError", "setRangeText allocation failed"))?;
        new_units.splice(range.0..range.1, replacement_units.iter().copied());
        let raw_value = lumen_common::smuggle::utf16_from_units(&new_units);
        if raw_value.len() > MAX_CONTROL_BYTES {
            return Err(OpError::new(
                "RangeError",
                "setRangeText result exceeds the text-control editing limit",
            ));
        }
        // Textarea selection operates on its API value. Normalize after the
        // code-unit splice so offsets are never calculated against a different
        // line-ending representation than the pre-edit value.
        let new_value = if is_textarea {
            normalize_textarea_value(&raw_value)?
        } else {
            raw_value
        };
        let inserted_end = range.0.saturating_add(replacement_units.len());

        forms::set_control_value(self, &mut self.forms.borrow_mut(), node, &new_value)?;

        let (selection_start, selection_end) = match selection_mode {
            "select" => (range.0, inserted_end),
            "start" => (range.0, range.0),
            "end" => (inserted_end, inserted_end),
            "preserve" => {
                let old_length = range.1 - range.0;
                let delta = replacement_units.len() as isize - old_length as isize;
                let mut selection_start = current_selection.0;
                let mut selection_end = current_selection.1;
                if use_current_selection {
                    if selection_start > range.1 {
                        selection_start = selection_start.saturating_add_signed(delta);
                    } else if selection_start > range.0 {
                        selection_start = range.0;
                    }
                    if selection_end > range.1 {
                        selection_end = selection_end.saturating_add_signed(delta);
                    } else if selection_end > range.0 {
                        selection_end = inserted_end;
                    }
                } else {
                    if selection_start > range.0 {
                        selection_start = range.0;
                    }
                    if selection_end > range.0 {
                        selection_end = inserted_end;
                    }
                }
                (selection_start, selection_end)
            }
            _ => unreachable!("selection mode was checked above"),
        };
        self.set_selection_and_queue_event(ctx, node, selection_start, selection_end, "none")
    }

    fn queue_select_event(self: &Rc<Self>, ctx: &mut Ctx, node: NodeId) -> OpResult<()> {
        let should_queue = self
            .editing
            .borrow_mut()
            .begin_select_event(node)
            .map_err(|_| OpError::new("QuotaExceededError", "selection event queue is full"))?;
        if !should_queue {
            return Ok(());
        }
        let target = self.wrap(ctx, node);
        let realm = self.clone();
        if let Err(error) = scheduling::queue_task(ctx, move |ctx| {
            let _target = target;
            realm.editing.borrow_mut().finish_select_event(node);
            realm.dispatch_user_agent(ctx, node, "select", true, false, &[])?;
            Ok(())
        }) {
            self.editing.borrow_mut().finish_select_event(node);
            return Err(error);
        }
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
        self.edit_control_key_with_keypress(ctx, node, properties, false, None)
    }

    pub(super) fn edit_control_key_with_keypress(
        self: &Rc<Self>,
        ctx: &mut Ctx,
        node: NodeId,
        properties: &[(&str, Value)],
        trusted_input: bool,
        keypress_properties: Option<&[(&str, Value)]>,
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
            let document = session.document();
            let tag = lumen_html::forms::html_element_local_name(document, node);
            let NodeKind::Element { .. } = document.kind(node).map_err(dom_error)? else {
                return Ok(());
            };
            let Some(tag) = tag else {
                return Ok(());
            };
            let attr = |key: &str| null_attribute(document, node, key);
            if !matches!(tag, "input" | "textarea") || attr("disabled").is_some() {
                return Ok(());
            }
            if tag == "input"
                && !matches!(
                    attr("type").unwrap_or("text"),
                    "text" | "search" | "email" | "url" | "tel" | "password" | "number"
                )
            {
                return Ok(());
            }
            (
                tag == "textarea",
                attr("maxlength").and_then(lumen_html::forms::parse_nonnegative_integer),
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
            let offset = lumen_common::smuggle::utf16_unit_len(&value[..next]);
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
        let left_utf16 = lumen_common::smuggle::utf16_unit_len(&value[..left]);
        let right_utf16 = lumen_common::smuggle::utf16_unit_len(&value[..right]);
        let data = (!inserted.is_empty()).then_some(inserted);
        let before = EditSnapshot {
            value,
            start,
            end,
            direction,
        };
        let _ = maximum;
        if trusted_input {
            self.apply_text_edit_with_keypress(
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
                true,
                if kind == "insertText" {
                    keypress_properties
                } else {
                    None
                },
            )?;
        } else {
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
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn eval_value_or_panic(engine: &mut Engine, source: &str) -> Value {
        match engine.eval_value(source).expect("JavaScript should parse") {
            Ok(value) => value,
            Err(thrown) => {
                let message = engine
                    .ctx()
                    .coerce_string(&thrown)
                    .map(|message| message.to_string())
                    .unwrap_or_else(|_| "unknown JavaScript exception".into());
                panic!("JavaScript threw while evaluating `{source}`: {message}");
            }
        }
    }

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
        assert!(matches!(
            engine
                .eval_value("field.value === 'aXz' && field.defaultValue === 'a😀z' && field.getAttribute('value') === 'a😀z'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        assert_eq!(realm.selection(node).unwrap().0, 2);
    }

    #[test]
    fn idl_value_changes_and_set_range_text_keep_utf16_selection_semantics() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<input id=field value=default><textarea id=area>default</textarea>",
            64,
        )
        .unwrap();

        let value_changes = eval_value_or_panic(
            &mut engine,
            r#"
                (() => {
                const field = document.getElementById('field');
                const area = document.getElementById('area');
                field.value = 'abcdef';
                field.setSelectionRange(2, 4);
                field.value = 'abcdef';
                const sameValueKeepsSelection = field.selectionStart === 2 && field.selectionEnd === 4;
                field.value = 'xy';
                const changedValueMovesCaret = field.selectionStart === 2 && field.selectionEnd === 2;
                field.type = 'button';
                const valueModeToDefaultMode = field.value === 'xy' && field.getAttribute('value') === 'xy';
                field.value = 'discarded';
                const defaultModeWritesContentAttribute = field.value === 'discarded' &&
                    field.getAttribute('value') === 'discarded';
                field.type = 'text';
                const defaultModeToValueMode = field.value === 'discarded';
                field.value = 'abc';
                field.setSelectionRange(1, 1);
                field.value = 'a\r\nbc';
                const sameSanitizedValueKeepsSelection = field.value === 'abc' &&
                    field.selectionStart === 1 && field.selectionEnd === 1;
                field.value = null;
                const nullInputIsEmpty = field.value === '';
                area.value = null;
                const nullTextareaIsEmpty = area.value === '';
                return [
                    ['same-value selection', sameValueKeepsSelection],
                    ['changed-value caret', changedValueMovesCaret],
                    ['value to default mode', valueModeToDefaultMode],
                    ['default-mode setter', defaultModeWritesContentAttribute],
                    ['default to value mode', defaultModeToValueMode],
                    ['sanitized equal value selection', sameSanitizedValueKeepsSelection],
                    ['null input', nullInputIsEmpty],
                    ['null textarea', nullTextareaIsEmpty]
                ].map(([name, passed]) => name + '=' + passed).join('|');
                })()
                "#,
        );
        assert!(
            matches!(&value_changes, Value::Str(value) if value.as_str() == "same-value selection=true|changed-value caret=true|value to default mode=true|default-mode setter=true|default to value mode=true|sanitized equal value selection=true|null input=true|null textarea=true"),
            "value/selection/type transition contract returned {}",
            match &value_changes {
                Value::Str(value) => value.as_str(),
                _ => "non-string result",
            }
        );

        let range_modes = eval_value_or_panic(
            &mut engine,
            r#"
                (() => {
                const field = document.getElementById('field');
                field.value = 'abcdef';
                field.setSelectionRange(2, 4);
                field.setRangeText('X');
                const oneArgumentPreserves = field.value === 'abXef' &&
                    field.selectionStart === 2 && field.selectionEnd === 3;
                field.setRangeText('Y', 1, 2, 'start');
                const startMode = field.value === 'aYXef' &&
                    field.selectionStart === 1 && field.selectionEnd === 1;
                field.setRangeText('Z', 1, 2, 'end');
                const endMode = field.value === 'aZXef' &&
                    field.selectionStart === 2 && field.selectionEnd === 2;
                field.setRangeText('Q', 1, 2, 'select');
                const selectMode = field.value === 'aQXef' &&
                    field.selectionStart === 1 && field.selectionEnd === 2;
                field.setSelectionRange(5, 6);
                field.setRangeText('P', 1, 2, 'preserve');
                const explicitPreserveMode = field.value === 'aPXef' &&
                    field.selectionStart === 1 && field.selectionEnd === 2;
                return [
                    ['one-argument preserve', oneArgumentPreserves],
                    ['start mode', startMode],
                    ['end mode', endMode],
                    ['select mode', selectMode],
                    ['explicit preserve mode', explicitPreserveMode]
                ].map(([name, passed]) => name + '=' + passed).join('|');
                })()
                "#,
        );
        assert!(
            matches!(&range_modes, Value::Str(value) if value.as_str() == "one-argument preserve=true|start mode=true|end mode=true|select mode=true|explicit preserve mode=true"),
            "setRangeText mode contract returned {}",
            match &range_modes {
                Value::Str(value) => value.as_str(),
                _ => "non-string result",
            }
        );

        let split_surrogate = eval_value_or_panic(
            &mut engine,
            r#"
                (() => {
                const field = document.getElementById('field');
                field.value = 'A😀Z';
                field.setRangeText('x', 2, 3, 'select');
                return [field.value.length, field.value.charCodeAt(1), field.value.charCodeAt(2),
                    field.selectionStart, field.selectionEnd].join(',');
                })()
                "#,
        );
        assert!(
            matches!(&split_surrogate, Value::Str(value) if value.as_str() == "4,55357,120,2,3"),
            "setRangeText UTF-16 code-unit result was {}",
            match &split_surrogate {
                Value::Str(value) => value.as_str(),
                _ => "non-string result",
            }
        );
    }

    #[test]
    fn set_range_text_queues_one_trusted_select_event_for_selection_changes() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<input id=field value=abcd>", 64).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');const events=[];field.addEventListener('select',e=>events.push([e.isTrusted,e.bubbles,e.cancelable,e.target===field]));field.setRangeText('X',1,2,'select');",
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(
            engine
                .eval_value("events.length===0")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        assert!(super::scheduling::run_tasks(&mut engine, 8).is_empty());
        assert!(matches!(
            engine
                .eval_value("events.length===1&&events[0].join(',')==='true,true,false,true'")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
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
    fn length_constraints_use_utf16_and_require_a_host_edit() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<input id=field minlength=2><input id=number type=number minlength=2><input id=long maxlength=4><textarea id=area minlength=2></textarea><textarea id=typed type=number minlength=2></textarea>",
            64,
        ).unwrap();
        engine
            .eval_value("field.value='x';area.value='x';number.value='x';typed.value='x'")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(engine.eval_value("!field.validity.tooShort && !area.validity.tooShort && !number.validity.tooShort && !typed.validity.tooShort").unwrap().ok().unwrap(), Value::Bool(true)));
        let field = {
            let session = realm.session.borrow();
            lumen_html::selector::query_selector(
                session.document(),
                session.document().root(),
                "#field",
            )
            .unwrap()
            .unwrap()
        };
        let area = {
            let session = realm.session.borrow();
            lumen_html::selector::query_selector(
                session.document(),
                session.document().root(),
                "#area",
            )
            .unwrap()
            .unwrap()
        };
        let long = {
            let session = realm.session.borrow();
            lumen_html::selector::query_selector(
                session.document(),
                session.document().root(),
                "#long",
            )
            .unwrap()
            .unwrap()
        };
        // A single supplementary character occupies two UTF-16 code units.
        engine
            .eval_value("field.value='';area.value=''")
            .unwrap()
            .ok()
            .unwrap();
        assert!(realm.host_insert_text(engine.ctx(), field, "😀").unwrap());
        assert!(realm.host_insert_text(engine.ctx(), area, "😀").unwrap());
        assert!(matches!(
            engine
                .eval_value("!field.validity.tooShort && !area.validity.tooShort")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        engine
            .eval_value("field.value='x';area.value='x'")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(
            engine
                .eval_value("!field.validity.tooShort && !area.validity.tooShort")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        engine.eval_value("field.value=''").unwrap().ok().unwrap();
        assert!(realm.host_insert_text(engine.ctx(), field, "x").unwrap());
        assert!(matches!(engine.eval_value("field.validity.tooShort && !field.checkValidity() && field.validationMessage.length>0").unwrap().ok().unwrap(), Value::Bool(true)));
        engine
            .eval_value("field.value='';field.oninput=()=>{field.value='x'}")
            .unwrap()
            .ok()
            .unwrap();
        assert!(realm.host_insert_text(engine.ctx(), field, "😀").unwrap());
        assert!(matches!(
            engine
                .eval_value("field.value==='x' && !field.validity.tooShort")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
        assert!(realm.host_insert_text(engine.ctx(), long, "😀").unwrap());
        engine
            .eval_value("long.setAttribute('maxlength','1')")
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(engine.eval_value("long.validity.tooLong && !long.checkValidity() && long.validationMessage.length>0").unwrap().ok().unwrap(), Value::Bool(true)));
        engine.eval_value("long.value='😀'").unwrap().ok().unwrap();
        assert!(matches!(
            engine
                .eval_value("!long.validity.tooLong")
                .unwrap()
                .ok()
                .unwrap(),
            Value::Bool(true)
        ));
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
