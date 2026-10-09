//! Shared bounded alignment grammar, serialization and physical positioning.
use super::{
    AlignItems, AlignmentKeyword as Keyword, Declaration, Direction, JustifyContent, Style, Value,
    WritingMode,
};
use alloc::{string::String, vec::Vec};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ItemAlignment {
    pub alignment: AlignItems,
    pub keyword: Option<Keyword>,
    pub safe: Option<bool>,
}

impl ItemAlignment {
    pub fn is_baseline(self) -> bool {
        self.alignment == AlignItems::Baseline
    }
    pub fn is_last_baseline(self) -> bool {
        self.is_baseline() && self.keyword == Some(Keyword::LastBaseline)
    }
    /// Physical left/top offset, before any flex placement-axis reversal.
    pub fn offset(
        self,
        free: f32,
        container: &Style,
        subject: &Style,
        horizontal: bool,
        flow_reversed: bool,
    ) -> f32 {
        let container_end = start_is_far(container, horizontal);
        let subject_end = start_is_far(subject, horizontal);
        let mut far = match self.keyword {
            Some(Keyword::FlexStart) => container_end ^ flow_reversed,
            Some(Keyword::FlexEnd) => !(container_end ^ flow_reversed),
            Some(Keyword::SelfStart | Keyword::FirstBaseline) => subject_end,
            Some(Keyword::SelfEnd | Keyword::LastBaseline) => !subject_end,
            Some(Keyword::Left | Keyword::LegacyLeft) if horizontal => false,
            Some(Keyword::Right | Keyword::LegacyRight) if horizontal => true,
            Some(Keyword::Left | Keyword::LegacyLeft | Keyword::Right | Keyword::LegacyRight) => {
                container_end
            }
            _ if self.is_baseline() => subject_end,
            _ if self.alignment == AlignItems::Stretch => container_end ^ flow_reversed,
            _ => {
                if self.alignment == AlignItems::End {
                    !container_end
                } else {
                    container_end
                }
            }
        };
        let center =
            self.alignment == AlignItems::Center || self.keyword == Some(Keyword::LegacyCenter);
        // Baseline fallback is safe self-start/end. Explicit unsafe remains
        // signed, including when the subject is larger than its container.
        if free < 0.0 && (self.safe == Some(true) || self.is_baseline()) {
            far = container_end;
            return if far { free } else { 0.0 };
        }
        if center {
            free * 0.5
        } else if far {
            free
        } else {
            0.0
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContentAlignment {
    pub alignment: JustifyContent,
    pub keyword: Option<Keyword>,
    pub safe: Option<bool>,
}
impl ContentAlignment {
    pub fn is_baseline(self) -> bool {
        matches!(
            self.keyword,
            Some(Keyword::FirstBaseline | Keyword::LastBaseline)
        )
    }
    pub fn is_last_baseline(self) -> bool {
        self.keyword == Some(Keyword::LastBaseline)
    }
    pub fn positional_offset(
        self,
        free: f32,
        container: &Style,
        horizontal: bool,
        flow_reversed: bool,
    ) -> Option<f32> {
        let alignment = if matches!(
            self.keyword,
            Some(Keyword::FirstBaseline | Keyword::LastBaseline)
        ) {
            AlignItems::Baseline
        } else {
            match self.alignment {
                JustifyContent::Start => AlignItems::Start,
                JustifyContent::End => AlignItems::End,
                JustifyContent::Center => AlignItems::Center,
                _ => return None,
            }
        };
        Some(
            ItemAlignment {
                alignment,
                keyword: self.keyword,
                safe: self.safe,
            }
            .offset(free, container, container, horizontal, flow_reversed),
        )
    }
}

fn start_is_far(style: &Style, horizontal: bool) -> bool {
    match (style.writing_mode, horizontal) {
        (WritingMode::HorizontalTb, true) => style.used_direction() == Direction::Rtl,
        (WritingMode::VerticalRl | WritingMode::SidewaysRl, true) => true,
        (WritingMode::VerticalLr | WritingMode::SidewaysLr, true) => false,
        (WritingMode::SidewaysLr, false) => style.used_direction() == Direction::Ltr,
        (WritingMode::HorizontalTb, false) => false,
        (_, false) => style.used_direction() == Direction::Rtl,
    }
}

impl Style {
    pub fn item_alignment(&self, horizontal: bool) -> ItemAlignment {
        if horizontal {
            ItemAlignment {
                alignment: self.justify_items,
                keyword: self.justify_items_keyword,
                safe: self.justify_items_safe,
            }
        } else {
            ItemAlignment {
                alignment: self.align_items,
                keyword: self.align_items_keyword,
                safe: self.align_items_safe,
            }
        }
    }
    pub fn self_alignment(&self, container: &Style, horizontal: bool) -> ItemAlignment {
        let (alignment, keyword, safe) = if horizontal {
            (
                self.justify_self,
                self.justify_self_keyword,
                self.justify_self_safe,
            )
        } else {
            (
                self.align_self,
                self.align_self_keyword,
                self.align_self_safe,
            )
        };
        let mut value = alignment.map_or_else(
            || container.item_alignment(horizontal),
            |alignment| ItemAlignment {
                alignment,
                keyword,
                safe,
            },
        );
        value.keyword = match value.keyword {
            Some(Keyword::LegacyLeft) => Some(Keyword::Left),
            Some(Keyword::LegacyRight) => Some(Keyword::Right),
            Some(Keyword::LegacyCenter) => None,
            Some(Keyword::Legacy) => Some(Keyword::Normal),
            keyword => keyword,
        };
        value
    }
    pub fn content_alignment(&self, horizontal: bool) -> ContentAlignment {
        if horizontal {
            ContentAlignment {
                alignment: self.justify_content,
                keyword: self.justify_content_keyword,
                safe: self.justify_content_safe,
            }
        } else {
            ContentAlignment {
                alignment: self.align_content.unwrap_or(JustifyContent::Stretch),
                keyword: self.align_content_keyword,
                safe: self.align_content_safe,
            }
        }
    }
    /// Canonical computed serialization shared by every CSSOM readback.
    pub fn alignment_css(&self, name: &str) -> Option<String> {
        if let Some((first, second)) = shorthand_axes(name) {
            let first = self.alignment_css(first)?;
            let second = self.alignment_css(second)?;
            return Some(if first == second {
                first
            } else {
                alloc::format!("{first} {second}")
            });
        }
        let slot = axis_slot(name)?;
        let (text, safe) = match slot {
            15 | 49 => {
                let value = self.content_alignment(slot == 15);
                (
                    value
                        .keyword
                        .map_or_else(|| content_text(value.alignment), Keyword::as_str),
                    value.safe,
                )
            }
            _ => {
                let (used, keyword, safe) = match slot {
                    16 => (
                        Some(self.align_items),
                        self.align_items_keyword,
                        self.align_items_safe,
                    ),
                    64 => (
                        Some(self.justify_items),
                        self.justify_items_keyword,
                        self.justify_items_safe,
                    ),
                    47 => (
                        self.align_self,
                        self.align_self_keyword,
                        self.align_self_safe,
                    ),
                    65 => (
                        self.justify_self,
                        self.justify_self_keyword,
                        self.justify_self_safe,
                    ),
                    _ => unreachable!(),
                };
                (
                    keyword.map_or_else(|| used.map_or("auto", item_text), Keyword::as_str),
                    safe,
                )
            }
        };
        Some(with_safety(text, safe))
    }
}

fn item_text(value: AlignItems) -> &'static str {
    match value {
        AlignItems::Stretch => "stretch",
        AlignItems::Start => "start",
        AlignItems::End => "end",
        AlignItems::Center => "center",
        AlignItems::Baseline => "baseline",
    }
}
fn content_text(value: JustifyContent) -> &'static str {
    match value {
        JustifyContent::Stretch => "stretch",
        JustifyContent::Start => "start",
        JustifyContent::End => "end",
        JustifyContent::Center => "center",
        JustifyContent::SpaceBetween => "space-between",
        JustifyContent::SpaceAround => "space-around",
        JustifyContent::SpaceEvenly => "space-evenly",
    }
}
fn with_safety(text: &str, safe: Option<bool>) -> String {
    match safe {
        Some(true) => alloc::format!("safe {text}"),
        Some(false) => alloc::format!("unsafe {text}"),
        None => String::from(text),
    }
}
fn axis_slot(name: &str) -> Option<usize> {
    Some(match name {
        "justify-content" => 15,
        "align-content" => 49,
        "align-items" => 16,
        "justify-items" => 64,
        "align-self" => 47,
        "justify-self" => 65,
        _ => return None,
    })
}
fn shorthand_axes(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "place-content" => Some(("align-content", "justify-content")),
        "place-items" => Some(("align-items", "justify-items")),
        "place-self" => Some(("align-self", "justify-self")),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Position {
    Auto,
    Normal,
    Stretch,
    Start,
    End,
    Center,
    FlowStart,
    FlowEnd,
    SelfStart,
    SelfEnd,
    Left,
    Right,
    Baseline,
    LastBaseline,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
    Legacy,
    LegacyLeft,
    LegacyRight,
    LegacyCenter,
}
#[derive(Clone, Copy)]
struct Parsed {
    position: Position,
    safe: Option<bool>,
    flex_alias: bool,
}
impl Parsed {
    fn specified(self) -> String {
        let text = match self.position {
            Position::Auto => "auto",
            Position::Normal => "normal",
            Position::Stretch => "stretch",
            Position::Start => "start",
            Position::End => "end",
            Position::Center => "center",
            Position::FlowStart if self.flex_alias => "flex-start",
            Position::FlowEnd if self.flex_alias => "flex-end",
            Position::FlowStart => "flow-start",
            Position::FlowEnd => "flow-end",
            Position::SelfStart => "self-start",
            Position::SelfEnd => "self-end",
            Position::Left => "left",
            Position::Right => "right",
            Position::Baseline => "baseline",
            Position::LastBaseline => "last baseline",
            Position::SpaceBetween => "space-between",
            Position::SpaceAround => "space-around",
            Position::SpaceEvenly => "space-evenly",
            Position::Legacy => "legacy",
            Position::LegacyLeft => "legacy left",
            Position::LegacyRight => "legacy right",
            Position::LegacyCenter => "legacy center",
        };
        with_safety(text, self.safe)
    }
    fn value(self, name: &str) -> Value {
        let slot = axis_slot(name).expect("validated alignment axis");
        let keyword = match self.position {
            Position::Normal => Some(Keyword::Normal),
            Position::FlowStart => Some(Keyword::FlexStart),
            Position::FlowEnd => Some(Keyword::FlexEnd),
            Position::SelfStart => Some(Keyword::SelfStart),
            Position::SelfEnd => Some(Keyword::SelfEnd),
            Position::Left => Some(Keyword::Left),
            Position::Right => Some(Keyword::Right),
            Position::Baseline if slot == 49 => Some(Keyword::FirstBaseline),
            Position::LastBaseline => Some(Keyword::LastBaseline),
            Position::Legacy => Some(Keyword::Legacy),
            Position::LegacyLeft => Some(Keyword::LegacyLeft),
            Position::LegacyRight => Some(Keyword::LegacyRight),
            Position::LegacyCenter => Some(Keyword::LegacyCenter),
            _ => None,
        };
        if matches!(slot, 15 | 49) {
            let alignment = match self.position {
                Position::Normal | Position::Stretch => None,
                Position::End | Position::FlowEnd | Position::Right | Position::LastBaseline => {
                    Some(JustifyContent::End)
                }
                Position::Center => Some(JustifyContent::Center),
                Position::SpaceBetween => Some(JustifyContent::SpaceBetween),
                Position::SpaceAround => Some(JustifyContent::SpaceAround),
                Position::SpaceEvenly => Some(JustifyContent::SpaceEvenly),
                _ => Some(JustifyContent::Start),
            };
            Value::ContentAlignment {
                slot,
                alignment,
                safe: self.safe,
                keyword,
            }
        } else {
            let alignment = match self.position {
                Position::Auto => None,
                Position::Normal | Position::Stretch | Position::Legacy => {
                    Some(AlignItems::Stretch)
                }
                Position::End
                | Position::FlowEnd
                | Position::SelfEnd
                | Position::Right
                | Position::LegacyRight => Some(AlignItems::End),
                Position::Center | Position::LegacyCenter => Some(AlignItems::Center),
                Position::Baseline | Position::LastBaseline => Some(AlignItems::Baseline),
                _ => Some(AlignItems::Start),
            };
            Value::ItemAlignment {
                slot,
                alignment,
                keyword,
                safe: self.safe,
            }
        }
    }
}

struct Tokens<'a> {
    values: [&'a str; 4],
    len: usize,
    next: usize,
}
impl<'a> Tokens<'a> {
    fn new(raw: &'a str) -> Option<Self> {
        if raw.len() > super::MAX_VARIABLE_BYTES {
            return None;
        }
        let mut result = Self {
            values: [""; 4],
            len: 0,
            next: 0,
        };
        for token in raw.split_ascii_whitespace() {
            if result.len == result.values.len() {
                return None;
            }
            result.values[result.len] = token;
            result.len += 1;
        }
        (result.len != 0).then_some(result)
    }
    fn take(&mut self) -> Option<&'a str> {
        let result = self
            .values
            .get(self.next)
            .copied()
            .filter(|_| self.next < self.len)?;
        self.next += 1;
        Some(result)
    }
    fn eat(&mut self, value: &str) -> bool {
        if self.next < self.len && self.values[self.next].eq_ignore_ascii_case(value) {
            self.next += 1;
            true
        } else {
            false
        }
    }
}
fn parse_axis(name: &str, tokens: &mut Tokens<'_>) -> Option<Parsed> {
    let slot = axis_slot(name)?;
    let content = matches!(slot, 15 | 49);
    let justify = matches!(slot, 15 | 64 | 65);
    let self_axis = matches!(slot, 47 | 65);
    let safe = if tokens.eat("safe") {
        Some(true)
    } else if tokens.eat("unsafe") {
        Some(false)
    } else {
        None
    };
    let token = tokens.take()?;
    let is = |keyword: &str| token.eq_ignore_ascii_case(keyword);
    let mut flex_alias = false;
    let position = if is("auto") && self_axis {
        Position::Auto
    } else if is("normal") {
        Position::Normal
    } else if is("stretch") {
        Position::Stretch
    } else if is("start") {
        Position::Start
    } else if is("end") {
        Position::End
    } else if is("center") {
        Position::Center
    } else if is("flow-start") || is("flex-start") {
        flex_alias = is("flex-start");
        Position::FlowStart
    } else if is("flow-end") || is("flex-end") {
        flex_alias = is("flex-end");
        Position::FlowEnd
    } else if is("self-start") && !content {
        Position::SelfStart
    } else if is("self-end") && !content {
        Position::SelfEnd
    } else if is("left") && justify {
        Position::Left
    } else if is("right") && justify {
        Position::Right
    } else if (is("baseline") || is("first") || is("last")) && slot != 15 {
        if !is("baseline") && !tokens.eat("baseline") {
            return None;
        }
        if is("last") {
            Position::LastBaseline
        } else {
            Position::Baseline
        }
    } else if is("space-between") && content {
        Position::SpaceBetween
    } else if is("space-around") && content {
        Position::SpaceAround
    } else if is("space-evenly") && content {
        Position::SpaceEvenly
    } else if is("legacy") && slot == 64 {
        if tokens.eat("left") {
            Position::LegacyLeft
        } else if tokens.eat("right") {
            Position::LegacyRight
        } else if tokens.eat("center") {
            Position::LegacyCenter
        } else {
            Position::Legacy
        }
    } else {
        return None;
    };
    let position = if slot == 64
        && safe.is_none()
        && matches!(
            position,
            Position::Left | Position::Right | Position::Center
        )
        && tokens.eat("legacy")
    {
        match position {
            Position::Left => Position::LegacyLeft,
            Position::Right => Position::LegacyRight,
            _ => Position::LegacyCenter,
        }
    } else {
        position
    };
    if safe.is_some()
        && !matches!(
            position,
            Position::Start
                | Position::End
                | Position::Center
                | Position::FlowStart
                | Position::FlowEnd
                | Position::SelfStart
                | Position::SelfEnd
                | Position::Left
                | Position::Right
        )
    {
        return None;
    }
    Some(Parsed {
        position,
        safe,
        flex_alias,
    })
}
fn parse(name: &str, raw: &str) -> Option<([(&'static str, Parsed); 2], usize)> {
    let mut tokens = Tokens::new(raw)?;
    let empty = Parsed {
        position: Position::Auto,
        safe: None,
        flex_alias: false,
    };
    let mut values = [("", empty); 2];
    let len = if let Some((first, second)) = shorthand_axes(name) {
        let value = parse_axis(first, &mut tokens)?;
        let other = if tokens.next == tokens.len {
            if name == "place-content"
                && matches!(value.position, Position::Baseline | Position::LastBaseline)
            {
                Parsed {
                    position: Position::Start,
                    safe: None,
                    flex_alias: false,
                }
            } else {
                value
            }
        } else {
            parse_axis(second, &mut tokens)?
        };
        values = [(first, value), (second, other)];
        2
    } else {
        let name = match axis_slot(name)? {
            15 => "justify-content",
            49 => "align-content",
            16 => "align-items",
            64 => "justify-items",
            47 => "align-self",
            65 => "justify-self",
            _ => unreachable!(),
        };
        values[0] = (name, parse_axis(name, &mut tokens)?);
        1
    };
    (tokens.next == tokens.len).then_some((values, len))
}
pub(super) fn value(name: &str, raw: &str) -> Option<Value> {
    let (values, len) = parse(name, raw)?;
    (len == 1).then(|| values[0].1.value(values[0].0))
}
pub(super) fn declarations(name: &str, raw: &str, important: bool) -> Option<[Declaration; 2]> {
    let (values, len) = parse(name, raw)?;
    (len == 2).then(|| {
        values.map(|(name, value)| Declaration {
            value: value.value(name),
            important,
        })
    })
}
/// Author declarations retain flex aliases; computed serialization uses flow aliases.
pub fn specified_alignment_expansion(name: &str, raw: &str) -> Option<Vec<(&'static str, String)>> {
    let (values, len) = parse(name, raw)?;
    Some(
        values[..len]
            .iter()
            .map(|(name, value)| (*name, value.specified()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_alignment_cursor_expands_full_values_and_rejects_partial_grammar() {
        for (name, raw, expected) in [
            (
                "place-self",
                "safe self-start unsafe self-end",
                [
                    ("align-self", "safe self-start"),
                    ("justify-self", "unsafe self-end"),
                ],
            ),
            (
                "place-items",
                "last baseline flex-start",
                [
                    ("align-items", "last baseline"),
                    ("justify-items", "flex-start"),
                ],
            ),
            (
                "place-items",
                "stretch right legacy",
                [
                    ("align-items", "stretch"),
                    ("justify-items", "legacy right"),
                ],
            ),
            (
                "place-items",
                "first baseline",
                [("align-items", "baseline"), ("justify-items", "baseline")],
            ),
            (
                "place-content",
                "last baseline",
                [
                    ("align-content", "last baseline"),
                    ("justify-content", "start"),
                ],
            ),
        ] {
            let actual = specified_alignment_expansion(name, raw).unwrap();
            assert_eq!(actual.len(), 2);
            for (actual, expected) in actual.iter().zip(expected) {
                assert_eq!((actual.0, actual.1.as_str()), expected);
            }
        }
        for (name, raw) in [
            ("align-self", "left"),
            ("align-content", "self-start"),
            ("justify-content", "baseline"),
            ("place-items", "safe stretch"),
            ("place-self", "legacy center"),
            ("place-items", "start safe"),
            ("place-content", "unsafe space-between"),
            ("place-items", "first baseline safe center extra"),
        ] {
            assert!(
                specified_alignment_expansion(name, raw).is_none(),
                "{name}: {raw}"
            );
        }
    }

    #[test]
    fn shared_alignment_positions_use_subject_container_flow_and_signed_safety() {
        let mut container = Style::initial();
        let mut subject = Style::initial();
        let position = |alignment, keyword, safe| ItemAlignment {
            alignment,
            keyword,
            safe,
        };
        assert_eq!(
            position(AlignItems::Start, None, None).offset(60.0, &container, &subject, true, true),
            0.0
        );
        assert_eq!(
            position(AlignItems::Start, Some(Keyword::FlexStart), None)
                .offset(60.0, &container, &subject, true, true),
            60.0
        );
        subject.direction = Direction::Rtl;
        assert_eq!(
            position(AlignItems::Start, Some(Keyword::SelfStart), None)
                .offset(60.0, &container, &subject, true, false),
            60.0
        );
        assert_eq!(
            position(AlignItems::End, Some(Keyword::SelfEnd), None)
                .offset(60.0, &container, &subject, true, false),
            0.0
        );
        assert_eq!(
            position(AlignItems::End, None, Some(false))
                .offset(-20.0, &container, &subject, true, false),
            -20.0
        );
        assert_eq!(
            position(AlignItems::End, None, Some(true))
                .offset(-20.0, &container, &subject, true, false),
            0.0
        );
        container.direction = Direction::Rtl;
        assert_eq!(
            position(AlignItems::Center, None, Some(true))
                .offset(-20.0, &container, &subject, true, false),
            -20.0
        );
        assert_eq!(
            position(AlignItems::Start, Some(Keyword::Left), None)
                .offset(60.0, &container, &subject, true, false),
            0.0
        );
        container.writing_mode = WritingMode::VerticalRl;
        subject.writing_mode = WritingMode::VerticalLr;
        assert_eq!(
            position(AlignItems::Start, None, None).offset(60.0, &container, &subject, true, false),
            60.0
        );
        assert_eq!(
            position(AlignItems::Start, Some(Keyword::SelfStart), None)
                .offset(60.0, &container, &subject, true, false),
            0.0
        );
        assert_eq!(
            position(AlignItems::Baseline, Some(Keyword::LastBaseline), None)
                .offset(60.0, &container, &subject, true, false),
            60.0
        );
        assert_eq!(
            position(AlignItems::Baseline, Some(Keyword::LastBaseline), None)
                .offset(-20.0, &container, &subject, true, false),
            -20.0
        );
    }
}
