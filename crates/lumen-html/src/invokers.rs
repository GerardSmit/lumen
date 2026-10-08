//! HTML command and popover invoker classification over the shared document.
use crate::{Document, NodeId};
use crate::forms::html_element_local_name;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command { Unknown, Custom, TogglePopover, ShowPopover, HidePopover, Close, RequestClose, ShowModal }

impl Command {
    pub fn parse(value: &str) -> Self {
        if value.starts_with("--") { return Self::Custom; }
        for (keyword, command) in [("toggle-popover", Self::TogglePopover), ("show-popover", Self::ShowPopover),
            ("hide-popover", Self::HidePopover), ("close", Self::Close), ("request-close", Self::RequestClose), ("show-modal", Self::ShowModal)] {
            if value.eq_ignore_ascii_case(keyword) { return command; }
        }
        Self::Unknown
    }
    pub fn reflected<'a>(value: &'a str) -> &'a str {
        match Self::parse(value) {
            Self::Unknown => "", Self::Custom => value,
            Self::TogglePopover => "toggle-popover", Self::ShowPopover => "show-popover", Self::HidePopover => "hide-popover",
            Self::Close => "close", Self::RequestClose => "request-close", Self::ShowModal => "show-modal",
        }
    }
    pub fn valid_for(self, document: &Document, target: NodeId) -> bool {
        match self {
            Self::Unknown => false,
            Self::Custom => true,
            Self::TogglePopover | Self::ShowPopover | Self::HidePopover => html_element_local_name(document, target).is_some(),
            Self::Close | Self::RequestClose | Self::ShowModal => html_element_local_name(document, target) == Some("dialog"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonState { Auto, Submit, Reset, Button }
pub fn button_state(document: &Document, node: NodeId) -> ButtonState {
    let value = document.get_attribute_ns_ref(node, None, "type").ok().flatten().unwrap_or("");
    if value.eq_ignore_ascii_case("submit") { ButtonState::Submit }
    else if value.eq_ignore_ascii_case("reset") { ButtonState::Reset }
    else if value.eq_ignore_ascii_case("button") { ButtonState::Button }
    else { ButtonState::Auto }
}
pub fn is_submit_button(document: &Document, node: NodeId) -> bool {
    match html_element_local_name(document, node) {
        Some("button") => match button_state(document, node) {
            ButtonState::Submit => true,
            ButtonState::Auto => document.get_attribute_ns_ref(node, None, "command").ok().flatten().is_none()
                && document.get_attribute_ns_ref(node, None, "commandfor").ok().flatten().is_none()
                && document.parent(node).ok().flatten().is_none_or(|parent| html_element_local_name(document, parent) != Some("select")),
            _ => false,
        },
        Some("input") => document.get_attribute_ns_ref(node, None, "type").ok().flatten()
            .is_some_and(|value| value.eq_ignore_ascii_case("submit") || value.eq_ignore_ascii_case("image")),
        _ => false,
    }
}
pub fn reflected_button_type(document: &Document, node: NodeId) -> &'static str {
    if is_submit_button(document, node) { "submit" }
    else if button_state(document, node) == ButtonState::Reset { "reset" }
    else { "button" }
}
pub fn is_button(document: &Document, node: NodeId) -> bool {
    match html_element_local_name(document, node) {
        Some("button") => true,
        Some("input") => document.get_attribute_ns_ref(node, None, "type").ok().flatten()
            .is_some_and(|value| ["button", "submit", "reset", "image"].iter().any(|keyword| value.eq_ignore_ascii_case(keyword))),
        _ => false,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverAction { Toggle, Show, Hide }
impl PopoverAction {
    pub fn parse(value: &str) -> Self {
        if value.eq_ignore_ascii_case("show") { Self::Show }
        else if value.eq_ignore_ascii_case("hide") { Self::Hide }
        else { Self::Toggle }
    }
    pub fn keyword(self) -> &'static str { match self { Self::Toggle => "toggle", Self::Show => "show", Self::Hide => "hide" } }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_window_invoker_auto_submit_and_command_target_classification() {
        let mut document = Document::new(128);
        let fragment = crate::html::parse_fragment(&mut document,
            "<button id=normal></button><button id=command command='--custom'></button><button id=target commandfor=x></button><button id=explicit type=SUBMIT command=close></button><select id=select></select><button id=inside></button><dialog id=d></dialog><div id=v></div><svg id=s></svg>").unwrap();
        let select=crate::selector::query_selector(&document,fragment,"#select").unwrap().unwrap();
        let inside=crate::selector::query_selector(&document,fragment,"#inside").unwrap().unwrap();
        document.append(select,inside).unwrap();
        let node = |name| crate::selector::query_selector(&document, fragment, name).unwrap().unwrap();
        assert!(is_submit_button(&document,node("#normal")));
        assert!(!is_submit_button(&document,node("#command")));
        assert!(!is_submit_button(&document,node("#target")));
        assert!(is_submit_button(&document,node("#explicit")));
        assert!(!is_submit_button(&document,node("#inside")));
        assert_eq!(reflected_button_type(&document,node("#target")),"button");
        assert!(Command::Close.valid_for(&document,node("#d")));
        assert!(!Command::Close.valid_for(&document,node("#v")));
        assert!(Command::TogglePopover.valid_for(&document,node("#v")));
        assert!(!Command::TogglePopover.valid_for(&document,node("#s")));
        assert!(Command::Custom.valid_for(&document,node("#s")));
        assert_eq!(Command::reflected("SHOW-MODAL"),"show-modal");
        assert_eq!(Command::reflected("--CaseSensitive"),"--CaseSensitive");
        assert_eq!(Command::reflected(" close"),"");
    }
}
