//! `text-spacing-trim`: fullwidth punctuation collapsing through the `chws` and
//! `halt` OpenType features.

use alloc::vec::Vec;
use lumen_html::paint::SpacingTrim;

#[derive(Clone, Copy, Eq, PartialEq)]
enum Class {
    Other,
    Open,
    Close,
    Middle,
}

fn class(ch: char) -> Class {
    match ch as u32 {
        0x3008 | 0x300A | 0x300C | 0x300E | 0x3010 | 0x3014 | 0x3016 | 0x3018 | 0x301A | 0x301D
        | 0xFF08 | 0xFF3B | 0xFF5B | 0xFF5F => Class::Open,
        0x3001 | 0x3002 | 0x3009 | 0x300B | 0x300D | 0x300F | 0x3011 | 0x3015 | 0x3017 | 0x3019
        | 0x301B | 0x301E | 0x301F | 0xFF09 | 0xFF0C | 0xFF0E | 0xFF3D | 0xFF5D | 0xFF60 => Class::Close,
        0x3000 | 0x30FB | 0xFF1A | 0xFF1B => Class::Middle,
        _ => Class::Other,
    }
}

/// Face support for contextual spacing features, as bits of `FontFace::spacing_features`.
pub(crate) const FACE_CHWS: u8 = 1;

pub(crate) fn needs_features(trim: SpacingTrim, text: &str) -> bool {
    trim != SpacingTrim::SpaceAll && text.chars().any(|ch| class(ch) != Class::Other)
}

/// Features for `text`; `before` and `after` are the characters surrounding it
/// in the same paragraph, so adjacency is not lost at font-run boundaries.
pub(crate) fn features(
    trim: SpacingTrim,
    face_features: u8,
    text: &str,
    before: Option<char>,
    after: Option<char>,
    out: &mut Vec<rustybuzz::Feature>,
) {
    if trim == SpacingTrim::SpaceAll {
        return;
    }
    if trim != SpacingTrim::TrimAll && face_features & FACE_CHWS != 0 {
        out.push(rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(b"chws"), 1, ..));
        return;
    }
    let halt = ttf_parser::Tag::from_bytes(b"halt");
    let mut previous = before.map_or(Class::Other, class);
    let mut chars = text.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        let current = class(ch);
        let next = chars.peek().map(|&(_, ch)| class(ch)).or_else(|| after.map(class)).unwrap_or(Class::Other);
        let trimmed = match (trim, current) {
            (_, Class::Other) => false,
            (SpacingTrim::TrimAll, _) => true,
            (_, Class::Open) => previous != Class::Other,
            (_, Class::Close) => matches!(next, Class::Close | Class::Middle),
            (_, Class::Middle) => false,
        };
        if trimmed {
            out.push(rustybuzz::Feature::new(halt, 1, start..start + ch.len_utf8()));
        }
        previous = current;
    }
}
