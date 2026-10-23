//! `text-align-last`: the alignment of the last line of a block and of lines
//! that end in a forced break.
use super::{extra_props, Style, TextAlign};

impl Style {
    pub fn line_text_align(&self, last: bool) -> TextAlign {
        if !last {
            return self.text_align;
        }
        match extra_props::keyword(self, "text-align-last") {
            Some("start" | "match-parent") => TextAlign::Start,
            Some("end") => TextAlign::End,
            Some("left") => TextAlign::Left,
            Some("right") => TextAlign::Right,
            Some("center") => TextAlign::Center,
            Some("justify") => TextAlign::JustifyAll,
            _ => self.text_align,
        }
    }
}
