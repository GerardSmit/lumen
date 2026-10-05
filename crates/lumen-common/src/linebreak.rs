//! Contextual corrections and the UAX #14 §8.2 Example 7 numeric tailoring
//! used by the official LineBreakTest corpus. Keep the provider's state machine
//! and property tables; only LB25, LB30 and unassigned LB30b need extra context.
use unicode_linebreak::{break_property, BreakClass as C, BreakOpportunity as B};

#[path = "linebreak_context.rs"]
mod data;

fn contains(ranges: &[(u32, u32)], cp: u32) -> bool {
    let index = ranges.partition_point(|&(_, end)| end < cp);
    ranges.get(index).is_some_and(|&(start, _)| cp >= start)
}

fn class(raw: C) -> C {
    match raw {
        C::Ambiguous | C::Unknown | C::Surrogate | C::ComplexContext => C::Alphabetic,
        C::ConditionalJapaneseStarter => C::NonStarter,
        _ => raw,
    }
}

pub(crate) fn opportunities(text: &str) -> impl Iterator<Item = (usize, B)> + '_ {
    let mut provider = unicode_linebreak::linebreaks(text).peekable();
    let mut previous = None;
    let mut previous_raw = C::Mandatory;
    let mut previous_cp = 0;
    let mut number = false;
    let mut closed_number = false;
    text.char_indices()
        .map(|(i, ch)| (i, Some(ch)))
        .chain(core::iter::once((text.len(), None)))
        .filter_map(move |(index, ch)| {
            let mut opportunity = if provider.peek().is_some_and(|&(at, _)| at == index) {
                provider.next().map(|(_, kind)| kind)
            } else {
                None
            };
            let Some(ch) = ch else {
                return opportunity.map(|kind| (index, kind));
            };
            let raw = break_property(ch as u32);
            let combining = matches!(raw, C::CombiningMark | C::ZeroWidthJoiner);
            let effective = if combining {
                previous
                    .filter(|p| {
                        !matches!(
                            p,
                            C::Mandatory
                                | C::CarriageReturn
                                | C::LineFeed
                                | C::NextLine
                                | C::Space
                                | C::ZeroWidthSpace
                        )
                    })
                    .unwrap_or(C::Alphabetic)
            } else {
                class(raw)
            };
            if let Some(left) = previous {
                if !combining && previous_raw != C::ZeroWidthJoiner {
                    let wide_open = raw == C::OpenPunctuation
                        && contains(data::WIDE, ch as u32)
                        && matches!(left, C::Alphabetic | C::HebrewLetter | C::Numeric);
                    let wide_close = left == C::CloseParenthesis
                        && contains(data::WIDE, previous_cp)
                        && matches!(effective, C::Alphabetic | C::HebrewLetter | C::Numeric);
                    let suffix_without_number =
                        matches!(left, C::ClosePunctuation | C::CloseParenthesis)
                            && matches!(effective, C::Postfix | C::Prefix)
                            && !closed_number;
                    let separator_without_number = matches!(left, C::InfixSeparator | C::Symbol)
                        && effective == C::Numeric
                        && !number;
                    let prefix_without_number = matches!(left, C::Prefix | C::Postfix)
                        && effective == C::OpenPunctuation
                        && text[index + ch.len_utf8()..]
                            .chars()
                            .map(|c| break_property(c as u32))
                            .find(|c| !matches!(c, C::CombiningMark | C::ZeroWidthJoiner))
                            != Some(C::Numeric);
                    if wide_open
                        || wide_close
                        || suffix_without_number
                        || separator_without_number
                        || prefix_without_number
                    {
                        opportunity = Some(B::Allowed);
                    }
                    if raw == C::EmojiModifier
                        && contains(data::UNASSIGNED_PICTOGRAPHIC, previous_cp)
                    {
                        opportunity = None;
                    }
                }
            }
            if !combining || previous.is_none() || effective != previous.unwrap() {
                closed_number =
                    number && matches!(effective, C::ClosePunctuation | C::CloseParenthesis);
                number = effective == C::Numeric
                    || number && matches!(effective, C::InfixSeparator | C::Symbol);
                previous_cp = ch as u32;
            }
            previous = Some(effective);
            previous_raw = raw;
            opportunity.map(|kind| (index, kind))
        })
}
