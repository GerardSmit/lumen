//! Trusted WebDriver key input for the host adapter.
//!
//! This is deliberately a small keyboard profile over the shared DOM event,
//! focus, and editing algorithms. It does not synthesize value assignments or
//! claim support for modifier/composition keys that the runtime cannot model.

use super::{DomRealm, NodeId};
use lumen::embed::{Ctx, OpError, Value};
use std::rc::Rc;

const MAX_KEY_INPUT_BYTES: usize = 64 * 1024;

/// A host can distinguish genuinely unsupported WebDriver keys from runtime
/// failures, even when page code catches an error from its own event handlers.
pub enum TrustedInputError {
    UnsupportedKey { codepoint: u32 },
    Operation(OpError),
}

impl core::fmt::Debug for TrustedInputError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnsupportedKey { codepoint } => formatter
                .debug_struct("UnsupportedKey")
                .field("codepoint", codepoint)
                .finish(),
            Self::Operation(_) => formatter.write_str("Operation(..)"),
        }
    }
}

impl From<OpError> for TrustedInputError {
    fn from(error: OpError) -> Self {
        Self::Operation(error)
    }
}

#[derive(Clone, Copy)]
enum KeyAction<'a> {
    Character(&'a str, char),
    Backspace,
    Tab,
    Enter,
    NumpadEnter,
    Home,
    End,
    ArrowLeft,
    ArrowRight,
    Delete,
    Shift,
    Control,
    Alt,
    Meta,
}

impl KeyAction<'_> {
    fn key(self) -> &'static str {
        match self {
            Self::Character(..) => "",
            Self::Backspace => "Backspace",
            Self::Tab => "Tab",
            Self::Enter => "Enter",
            Self::NumpadEnter => "Enter",
            Self::Home => "Home",
            Self::End => "End",
            Self::ArrowLeft => "ArrowLeft",
            Self::ArrowRight => "ArrowRight",
            Self::Delete => "Delete",
            Self::Shift => "Shift",
            Self::Control => "Control",
            Self::Alt => "Alt",
            Self::Meta => "Meta",
        }
    }

    fn needs_text_control(self) -> bool {
        !matches!(
            self,
            Self::Tab | Self::Shift | Self::Control | Self::Alt | Self::Meta
        )
    }

    fn is_character(self) -> bool {
        matches!(self, Self::Character(..))
    }
}

fn decode_key<'a>(input: &'a str, ch: char) -> Result<KeyAction<'a>, TrustedInputError> {
    let action = match ch {
        // WebDriver's private-use key values from the normative key table.
        '\u{e003}' => KeyAction::Backspace,
        '\u{e004}' | '\t' => KeyAction::Tab,
        '\u{e006}' => KeyAction::NumpadEnter,
        '\u{e007}' | '\r' | '\n' => KeyAction::Enter,
        '\u{e00d}' => KeyAction::Character(" ", ' '),
        '\u{e010}' => KeyAction::End,
        '\u{e011}' => KeyAction::Home,
        '\u{e012}' => KeyAction::ArrowLeft,
        '\u{e014}' => KeyAction::ArrowRight,
        '\u{e017}' => KeyAction::Delete,
        '\u{e008}' => KeyAction::Shift,
        '\u{e009}' => KeyAction::Control,
        '\u{e00a}' => KeyAction::Alt,
        '\u{e03d}' => KeyAction::Meta,
        ch if ch.is_control() || (0xe000..=0xf8ff).contains(&(ch as u32)) => {
            return Err(TrustedInputError::UnsupportedKey {
                codepoint: ch as u32,
            });
        }
        ch => KeyAction::Character(&input[..ch.len_utf8()], ch),
    };
    Ok(action)
}

fn code_for_character(ch: char) -> &'static str {
    match ch {
        'a'..='z' | 'A'..='Z' => match ch.to_ascii_uppercase() {
            'A' => "KeyA",
            'B' => "KeyB",
            'C' => "KeyC",
            'D' => "KeyD",
            'E' => "KeyE",
            'F' => "KeyF",
            'G' => "KeyG",
            'H' => "KeyH",
            'I' => "KeyI",
            'J' => "KeyJ",
            'K' => "KeyK",
            'L' => "KeyL",
            'M' => "KeyM",
            'N' => "KeyN",
            'O' => "KeyO",
            'P' => "KeyP",
            'Q' => "KeyQ",
            'R' => "KeyR",
            'S' => "KeyS",
            'T' => "KeyT",
            'U' => "KeyU",
            'V' => "KeyV",
            'W' => "KeyW",
            'X' => "KeyX",
            'Y' => "KeyY",
            _ => "KeyZ",
        },
        '0' | ')' => "Digit0",
        '1' | '!' => "Digit1",
        '2' | '@' => "Digit2",
        '3' | '#' => "Digit3",
        '4' | '$' => "Digit4",
        '5' | '%' => "Digit5",
        '6' | '^' => "Digit6",
        '7' | '&' => "Digit7",
        '8' | '*' => "Digit8",
        '9' | '(' => "Digit9",
        ' ' => "Space",
        '-' | '_' => "Minus",
        '=' | '+' => "Equal",
        '[' | '{' => "BracketLeft",
        ']' | '}' => "BracketRight",
        '\\' | '|' => "Backslash",
        ';' | ':' => "Semicolon",
        '\'' | '"' => "Quote",
        ',' | '<' => "Comma",
        '.' | '>' => "Period",
        '/' | '?' => "Slash",
        '`' | '~' => "Backquote",
        _ => "",
    }
}

fn code_for_action(action: KeyAction<'_>) -> &'static str {
    match action {
        KeyAction::Character(_, ch) => code_for_character(ch),
        KeyAction::Backspace => "Backspace",
        KeyAction::Tab => "Tab",
        KeyAction::Enter => "Enter",
        KeyAction::NumpadEnter => "NumpadEnter",
        KeyAction::Home => "Home",
        KeyAction::End => "End",
        KeyAction::ArrowLeft => "ArrowLeft",
        KeyAction::ArrowRight => "ArrowRight",
        KeyAction::Delete => "Delete",
        KeyAction::Shift => "ShiftLeft",
        KeyAction::Control => "ControlLeft",
        KeyAction::Alt => "AltLeft",
        KeyAction::Meta => "MetaLeft",
    }
}

fn key_for_action(action: KeyAction<'_>) -> &str {
    match action {
        KeyAction::Character(text, _) => text,
        _ => action.key(),
    }
}

fn legacy_key_code(action: KeyAction<'_>) -> u32 {
    match action {
        KeyAction::Character(_, ch) if ch.is_ascii_alphabetic() => ch.to_ascii_uppercase() as u32,
        KeyAction::Character(_, ch) if ch.is_ascii() => ch as u32,
        KeyAction::Backspace => 8,
        KeyAction::Tab => 9,
        KeyAction::Enter => 13,
        KeyAction::NumpadEnter => 13,
        KeyAction::Home => 36,
        KeyAction::End => 35,
        KeyAction::ArrowLeft => 37,
        KeyAction::ArrowRight => 39,
        KeyAction::Delete => 46,
        KeyAction::Shift => 16,
        KeyAction::Control => 17,
        KeyAction::Alt => 18,
        KeyAction::Meta => 91,
        _ => 0,
    }
}

fn shift_for_action(action: KeyAction<'_>) -> bool {
    match action {
        KeyAction::Character(_, ch) => {
            ch.is_ascii_uppercase() || "!@#$%^&*()_+{}|:\"<>?~".contains(ch)
        }
        KeyAction::Shift => true,
        _ => false,
    }
}

fn location_for_action(action: KeyAction<'_>) -> u32 {
    if matches!(action, KeyAction::NumpadEnter) {
        3
    } else {
        0
    }
}

fn keyboard_properties(action: KeyAction<'_>, kind: &str) -> [(&'static str, Value); 13] {
    let key_code = legacy_key_code(action);
    let char_code = if kind == "keypress" {
        match action {
            KeyAction::Character(_, ch) => ch as u32,
            _ => 0,
        }
    } else {
        0
    };
    [
        ("key", Value::str(key_for_action(action))),
        ("code", Value::str(code_for_action(action))),
        ("location", Value::Num(location_for_action(action) as f64)),
        ("repeat", Value::Bool(false)),
        ("isComposing", Value::Bool(false)),
        ("charCode", Value::Num(char_code as f64)),
        (
            "keyCode",
            Value::Num((if char_code != 0 { char_code } else { key_code }) as f64),
        ),
        (
            "which",
            Value::Num((if char_code != 0 { char_code } else { key_code }) as f64),
        ),
        ("ctrlKey", Value::Bool(false)),
        ("shiftKey", Value::Bool(shift_for_action(action))),
        ("altKey", Value::Bool(false)),
        ("metaKey", Value::Bool(false)),
        ("modifierAltGraph", Value::Bool(false)),
    ]
}

/// Validate one WebDriver `keyDown`/`keyUp` key before dispatching any events in a sequence.
pub(crate) fn validate_action_key(key: &str) -> Result<(), TrustedInputError> {
    let mut chars = key.chars();
    let Some(ch) = chars.next() else {
        return Err(TrustedInputError::UnsupportedKey { codepoint: 0 });
    };
    if chars.next().is_some() {
        return Err(TrustedInputError::UnsupportedKey {
            codepoint: ch as u32,
        });
    }
    decode_key(key, ch).map(|_| ())
}

fn action_modifiers(action: KeyAction<'_>, held: &[String]) -> (bool, bool, bool, bool) {
    let has = |key: &str| held.iter().any(|held| held == key);
    (
        has("\u{e009}"),
        has("\u{e008}") || shift_for_action(action),
        has("\u{e00a}"),
        has("\u{e03d}"),
    )
}

fn action_keyboard_properties(
    action: KeyAction<'_>,
    kind: &str,
    held: &[String],
    repeat: bool,
) -> [(&'static str, Value); 13] {
    let mut properties = keyboard_properties(action, kind);
    let (ctrl, shift, alt, meta) = action_modifiers(action, held);
    properties[3].1 = Value::Bool(repeat);
    properties[8].1 = Value::Bool(ctrl);
    properties[9].1 = Value::Bool(shift);
    properties[10].1 = Value::Bool(alt);
    properties[11].1 = Value::Bool(meta);
    properties
}

fn action_event_target(realm: &DomRealm) -> NodeId {
    realm
        .focused_node()
        .unwrap_or_else(|| realm.session.borrow().document().root())
}

/// Deliver a single held-key transition. The caller owns the per-realm pressed-key set and passes
/// the modifier state that should be visible during this event.
pub(crate) fn trusted_action_key_down(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    key: &str,
    held_after: &[String],
    repeat: bool,
) -> Result<(), TrustedInputError> {
    validate_action_key(key)?;
    let ch = key.chars().next().expect("validated action key");
    let action = decode_key(key, ch)?;
    let target = action_event_target(realm);
    if action.needs_text_control() {
        let (text_control, _) = is_text_control(realm, target)?;
        let implicit_enter = matches!(action, KeyAction::Enter | KeyAction::NumpadEnter)
            && implicit_submission(realm, target).is_some();
        if !text_control && !implicit_enter {
            return Err(TrustedInputError::UnsupportedKey {
                codepoint: ch as u32,
            });
        }
    }
    realm.mark_user_activation();
    let down = action_keyboard_properties(action, "keydown", held_after, repeat);
    let allowed = realm.dispatch_user_agent_keyboard_event(ctx, target, "keydown", &down)?;
    if allowed && realm.focused_node() == Some(target) {
        let keypress = action
            .is_character()
            .then(|| action_keyboard_properties(action, "keypress", held_after, repeat));
        realm.edit_control_key_with_keypress(
            ctx,
            target,
            &down,
            true,
            keypress.as_ref().map(|properties| &properties[..]),
        )?;
        if matches!(action, KeyAction::Enter | KeyAction::NumpadEnter) {
            perform_implicit_submission(ctx, realm, target)?;
        }
    }
    Ok(())
}

pub(crate) fn trusted_action_key_up(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    key: &str,
    held_after: &[String],
) -> Result<(), TrustedInputError> {
    validate_action_key(key)?;
    let ch = key.chars().next().expect("validated action key");
    let action = decode_key(key, ch)?;
    let target = action_event_target(realm);
    let up = action_keyboard_properties(action, "keyup", held_after, false);
    realm.dispatch_user_agent_keyboard_event(ctx, target, "keyup", &up)?;
    Ok(())
}

fn is_text_control(realm: &DomRealm, node: NodeId) -> Result<(bool, bool), OpError> {
    let session = realm.session_handle();
    let session = session.borrow();
    let document = session.document();
    let Some(name) = lumen_html::forms::html_element_local_name(document, node) else {
        return Ok((false, false));
    };
    if name == "textarea" {
        return Ok((true, true));
    }
    if name != "input" {
        return Ok((false, false));
    }
    let input_type = document
        .get_attribute_ns_ref(node, None, "type")
        .map_err(super::dom_error)?
        .unwrap_or("text");
    let editable = [
        "text", "search", "email", "url", "tel", "password", "number",
    ]
    .iter()
    .any(|state| input_type.eq_ignore_ascii_case(state));
    Ok((editable, false))
}

fn implicit_submission(
    realm: &DomRealm,
    node: NodeId,
) -> Option<lumen_html::forms::ImplicitSubmission> {
    let session = realm.session.borrow();
    lumen_html::forms::implicit_submission(session.document(), node)
}

fn perform_implicit_submission(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    input: NodeId,
) -> Result<(), OpError> {
    let (current_realm, current_input) = realm.resolve_adopted_node(input);
    let action = implicit_submission(&current_realm, current_input);
    match action {
        Some(lumen_html::forms::ImplicitSubmission::ClickDefaultButton { button, .. }) => {
            let (button_realm, button) = current_realm.resolve_adopted_node(button);
            super::forms::activate_keyboard_default_button(ctx, &button_realm, button)?;
        }
        Some(lumen_html::forms::ImplicitSubmission::SubmitForm(form)) => {
            let (form_realm, form) = current_realm.resolve_adopted_node(form);
            form_realm.submit_form(ctx, form, None)?;
        }
        None => {}
    }
    Ok(())
}

/// Send a bounded key string to a focused HTML text control using trusted DOM
/// KeyboardEvents and the existing editing default-action path.
///
/// The implementation accepts printable Unicode scalar values plus the
/// Backspace, Tab, Enter (textarea), Home, End, Left/Right, and Delete WebDriver
/// keys. Modifier, composition, clipboard, and other PUA keys return
/// `UnsupportedKey` rather than being silently dropped.
pub fn trusted_send_keys(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    keys: &str,
) -> Result<(), TrustedInputError> {
    if keys.len() > MAX_KEY_INPUT_BYTES {
        return Err(OpError::new("RangeError", "key input exceeds the input limit").into());
    }
    // Validate every PUA/control key before focusing or dispatching anything;
    // an unsupported suffix must not leave a partially typed value behind.
    for (offset, ch) in keys.char_indices() {
        let action = decode_key(&keys[offset..], ch)?;
        if matches!(
            action,
            KeyAction::Shift | KeyAction::Control | KeyAction::Alt | KeyAction::Meta
        ) {
            return Err(TrustedInputError::UnsupportedKey {
                codepoint: ch as u32,
            });
        }
    }

    realm.note_keyboard_modality();
    realm.focus(ctx, Some(node))?;
    if realm.focused_node() != Some(node) {
        return Err(OpError::new(
            "InvalidStateError",
            "key input target could not receive focus",
        )
        .into());
    }

    for (offset, ch) in keys.char_indices() {
        let action = decode_key(&keys[offset..], ch)?;
        let event_target = realm.focused_node().unwrap_or(node);
        if action.needs_text_control() {
            let (is_text_control, _) = is_text_control(realm, event_target)?;
            let implicit_enter = matches!(action, KeyAction::Enter | KeyAction::NumpadEnter)
                && implicit_submission(realm, event_target).is_some();
            if !is_text_control && !implicit_enter {
                return Err(TrustedInputError::UnsupportedKey {
                    codepoint: ch as u32,
                });
            }
        }

        let down = keyboard_properties(action, "keydown");
        let default_action = (|| -> Result<(), OpError> {
            // All keys admitted by this profile trigger user activation; Escape
            // and reserved shortcut/modifier keys are rejected during preflight.
            // Grant it before keydown listeners, including listeners that cancel.
            realm.mark_user_activation();
            let down_allowed =
                realm.dispatch_user_agent_keyboard_event(ctx, event_target, "keydown", &down)?;
            if down_allowed && realm.focused_node() == Some(event_target) {
                let keypress = action
                    .is_character()
                    .then(|| keyboard_properties(action, "keypress"));
                realm.edit_control_key_with_keypress(
                    ctx,
                    event_target,
                    &down,
                    true,
                    keypress.as_ref().map(|properties| &properties[..]),
                )?;
                if matches!(action, KeyAction::Enter | KeyAction::NumpadEnter) {
                    perform_implicit_submission(ctx, realm, event_target)?;
                }
            }
            Ok(())
        })();

        let keyup_target = realm.focused_node().unwrap_or(event_target);
        let up = keyboard_properties(action, "keyup");
        let keyup = realm.dispatch_user_agent_keyboard_event(ctx, keyup_target, "keyup", &up);
        default_action?;
        keyup?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    fn evaluate(engine: &mut Engine, source: &str) -> Value {
        engine
            .eval_value(source)
            .expect("source parses")
            .unwrap_or_else(|_| panic!("source evaluates"))
    }

    fn element(realm: &DomRealm, id: &str) -> NodeId {
        let session = realm.session_handle();
        let session = session.borrow();
        let document = session.document();
        lumen_html::selector::get_element_by_id(document, document.root(), id)
            .expect("valid id lookup")
            .expect("element exists")
    }

    #[test]
    fn trusted_send_keys_dispatches_keyboard_and_edit_events_in_order() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<input id=field>", 32).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');const events=[];\
                 for(const type of ['keydown','beforeinput','keypress','input','keyup'])\
                   field.addEventListener(type,e=>events.push(type+':' +(e instanceof KeyboardEvent)+ ':' +e.isTrusted+\
                     (e instanceof KeyboardEvent ? ':'+(e.view===window)+':'+e.which : '')));",
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("listener setup evaluates"));
        let node = element(&realm, "field");

        trusted_send_keys(engine.ctx(), &realm, node, "A").unwrap();
        assert!(realm.has_transient_user_activation());
        assert!(realm.has_been_active());

        assert!(matches!(
            evaluate(&mut engine, "field.value + '|' + events.join(',')"),
            Value::Str(value)
                if value.as_str() == "A|keydown:true:true:true:65,beforeinput:false:true,keypress:true:true:true:65,input:false:true,keyup:true:true:true:65"
        ));
    }

    #[test]
    fn trusted_backspace_uses_the_shared_beforeinput_and_input_path() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<input id=field value=ab>", 32).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');field.focus();\
                 field.setSelectionRange(2,2);const events=[];\
                 for(const type of ['beforeinput','input'])field.addEventListener(type,e=>\
                   events.push(type+':'+(e instanceof InputEvent)+':'+e.inputType+':'+String(e.data)+\
                     ':'+e.isTrusted+':'+e.cancelable+':'+e.isComposing+':'+(e.view===window)));",
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("listener setup evaluates"));
        let node = element(&realm, "field");

        trusted_send_keys(engine.ctx(), &realm, node, "\u{e003}").unwrap();

        assert!(matches!(
            evaluate(&mut engine, "field.value + '|' + events.join(',')"),
            Value::Str(value)
                if value.as_str() == "a|beforeinput:true:deleteContentBackward:null:true:true:false:true,input:true:deleteContentBackward:null:true:false:false:true"
        ));
    }

    #[test]
    fn canceled_keydown_keeps_value_and_still_dispatches_keyup() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<input id=field>", 32).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');const events=[];\
                 field.addEventListener('keydown',e=>{events.push('down');e.preventDefault()});\
                 field.addEventListener('beforeinput',()=>events.push('before'));\
                 field.addEventListener('keyup',()=>events.push('up'));",
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("listener setup evaluates"));
        let node = element(&realm, "field");

        trusted_send_keys(engine.ctx(), &realm, node, "x").unwrap();
        assert!(realm.has_transient_user_activation());

        assert!(matches!(
            evaluate(&mut engine, "field.value + '|' + events.join(',')"),
            Value::Str(value) if value.as_str() == "|down,up"
        ));
    }

    #[test]
    fn unsupported_modifier_is_rejected_before_partial_input() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<input id=field>", 32).unwrap();
        engine
            .eval_value(
                "const field=document.getElementById('field');let events=0;\
                 field.addEventListener('keydown',()=>events++);",
            )
            .unwrap()
            .unwrap_or_else(|_| panic!("listener setup evaluates"));
        let node = element(&realm, "field");

        assert!(matches!(
            trusted_send_keys(engine.ctx(), &realm, node, "x\u{e008}"),
            Err(TrustedInputError::UnsupportedKey { codepoint: 0xe008 })
        ));
        assert!(matches!(
            evaluate(&mut engine, "field.value === '' && events === 0"),
            Value::Bool(true)
        ));
    }
}
